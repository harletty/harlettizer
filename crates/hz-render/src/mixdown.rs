//! A programme rendered onto a channel layout, sample for sample.
//!
//! Each source is either a speaker — a bed channel the layout has, fed
//! straight through at unity — or an object, whose gains the [`Room`] sets
//! at each update and which ramps to them over the samples the update asks
//! for. Objects are panned on the room's cube rather than by direction, so
//! that a master's corners are the layout's speakers: see [`crate::room`].
//!
//! **Gains ramp, positions do not**: the room is asked once per update, as
//! the panner is everywhere else in this workspace, and an object moving
//! across a ramp passes along the chord between its two gain vectors rather
//! than through the positions between. Panning per sample would buy nothing
//! audible for its cost.
//!
//! What the room does not honour is not honoured here either: an object's
//! size, and its rendering mode — snapping to a speaker, zone exclusions,
//! screen scaling — which are statements to a decoder's renderer, and a bed
//! has no renderer after it. Every object renders as a free point.

use crate::{Keyframe, Layout, Room};
use hz_core::Result;

/// Renders sources onto one layout.
pub struct Mixdown {
    room: Room,
    channels: usize,
    sources: Vec<Source>,
}

struct Source {
    /// Where its signal is in an interleaved input frame.
    input: usize,
    route: Route,
}

enum Route {
    /// Straight to one channel at unity.
    Speaker(usize),
    Object {
        gains: Vec<f64>,
        target: Vec<f64>,
        step: Vec<f64>,
        /// Samples left until `gains` reaches `target`.
        remaining: u32,
    },
}

impl Mixdown {
    pub fn new(layout: &Layout) -> Result<Self> {
        Ok(Self {
            room: Room::new(layout)?,
            channels: layout.channels(),
            sources: Vec::new(),
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// A source read from input channel `input` and fed to `channel` as it
    /// is. Returns its index.
    pub fn speaker(&mut self, input: usize, channel: usize) -> usize {
        assert!(
            channel < self.channels,
            "channel {channel} is not in the layout"
        );
        self.sources.push(Source {
            input,
            route: Route::Speaker(channel),
        });
        self.sources.len() - 1
    }

    /// An object read from input channel `input`, silent until its first
    /// [`Mixdown::update`]. Returns its index.
    pub fn object(&mut self, input: usize) -> usize {
        self.sources.push(Source {
            input,
            route: Route::Object {
                gains: vec![0.0; self.channels],
                target: vec![0.0; self.channels],
                step: vec![0.0; self.channels],
                remaining: 0,
            },
        });
        self.sources.len() - 1
    }

    /// Move object `source` to `state`: its gains start ramping towards the
    /// room's for the new position, scaled by its gain, and get there
    /// after `state.ramp_samples`. A ramp still running is abandoned where
    /// it stands, which is what a renderer handed a new update does.
    pub fn update(&mut self, source: usize, state: &Keyframe) {
        let Route::Object {
            gains,
            target,
            step,
            remaining,
        } = &mut self.sources[source].route
        else {
            panic!("source {source} is a speaker, which does not move");
        };
        self.room.gains(state.position, target.as_mut_slice());
        for t in target.iter_mut() {
            *t *= state.gain;
        }
        if state.ramp_samples == 0 {
            gains.copy_from_slice(target);
            *remaining = 0;
        } else {
            let length = f64::from(state.ramp_samples);
            for ((s, &g), &t) in step.iter_mut().zip(gains.iter()).zip(target.iter()) {
                *s = (t - g) / length;
            }
            *remaining = state.ramp_samples;
        }
    }

    /// Add `frames` frames of `input` — interleaved, `stride` channels a
    /// frame — into `out`, interleaved in layout order. `out` is added to,
    /// not overwritten, so that a block split at its updates is rendered
    /// span by span into one buffer.
    pub fn render(&mut self, input: &[f32], stride: usize, frames: usize, out: &mut [f64]) {
        let channels = self.channels;
        assert!(input.len() >= frames * stride, "the input is short");
        assert!(out.len() >= frames * channels, "the output is short");
        for source in &mut self.sources {
            let sample = |i: usize| f64::from(input[i * stride + source.input]);
            match &mut source.route {
                Route::Speaker(channel) => {
                    for i in 0..frames {
                        out[i * channels + *channel] += sample(i);
                    }
                }
                Route::Object {
                    gains,
                    target,
                    step,
                    remaining,
                } => {
                    // The ramp, for as much of it as falls in this span …
                    let ramped = (*remaining as usize).min(frames);
                    for i in 0..ramped {
                        let x = sample(i);
                        let frame = &mut out[i * channels..(i + 1) * channels];
                        for ((o, g), s) in frame.iter_mut().zip(gains.iter_mut()).zip(step.iter()) {
                            *o += x * *g;
                            *g += *s;
                        }
                    }
                    *remaining -= ramped as u32;
                    // … landing exactly on the target rather than on the sum
                    // of its steps …
                    if *remaining == 0 {
                        gains.copy_from_slice(target);
                    }
                    // … and the rest at a standstill.
                    if gains.iter().all(|&g| g == 0.0) {
                        continue;
                    }
                    for i in ramped..frames {
                        let x = sample(i);
                        let frame = &mut out[i * channels..(i + 1) * channels];
                        for (o, g) in frame.iter_mut().zip(gains.iter()) {
                            *o += x * *g;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(position: [f64; 3], ramp_samples: u32) -> Keyframe {
        Keyframe {
            position,
            ramp_samples,
            ..Keyframe::default()
        }
    }

    /// A speaker source is copied, an object on a speaker lands on it alone.
    #[test]
    fn a_speaker_is_copied_and_an_object_on_one_lands_there() {
        let layout = Layout::surround_7_1_4();
        let mut mixdown = Mixdown::new(&layout).unwrap();
        mixdown.speaker(0, 3);
        let object = mixdown.object(1);
        // Front left is at negative X.
        mixdown.update(object, &at([-1.0, 1.0, 0.0], 0));
        let input = [0.5f32, 0.25, -0.5, 0.25];
        let mut out = vec![0.0; 2 * 12];
        mixdown.render(&input, 2, 2, &mut out);
        assert_eq!(out[3], 0.5);
        assert_eq!(out[12 + 3], -0.5);
        let left = layout.index_of("M+030").unwrap();
        assert!((out[left] - 0.25).abs() < 1e-9, "{:?}", &out[..12]);
        let elsewhere: f64 = out[..12]
            .iter()
            .enumerate()
            .filter(|&(c, _)| c != left && c != 3)
            .map(|(_, v)| v.abs())
            .sum();
        assert!(elsewhere < 1e-9, "{:?}", &out[..12]);
    }

    /// A ramp passes through the midpoint halfway and stops on the target,
    /// across spans that split it anywhere.
    #[test]
    fn a_ramp_lands_on_its_target_across_spans() {
        let layout = Layout::surround_7_1_4();
        let left = layout.index_of("M+030").unwrap();
        let right = layout.index_of("M-030").unwrap();
        let mut mixdown = Mixdown::new(&layout).unwrap();
        let object = mixdown.object(0);
        mixdown.update(object, &at([-1.0, 1.0, 0.0], 0));
        mixdown.update(object, &at([1.0, 1.0, 0.0], 100));
        let input = vec![1.0f32; 300];
        let mut out = vec![0.0; 300 * 12];
        // Three spans, the first ending inside the ramp.
        mixdown.render(&input[..37], 1, 37, &mut out[..37 * 12]);
        mixdown.render(&input[37..250], 1, 213, &mut out[37 * 12..250 * 12]);
        mixdown.render(&input[250..], 1, 50, &mut out[250 * 12..]);
        let frame = |i: usize| &out[i * 12..(i + 1) * 12];
        assert!((frame(50)[left] - 0.5).abs() < 1e-9);
        assert!((frame(50)[right] - 0.5).abs() < 1e-9);
        assert_eq!(frame(100)[left], 0.0);
        assert_eq!(frame(100)[right], 1.0);
        assert_eq!(frame(299)[right], 1.0);
    }
}
