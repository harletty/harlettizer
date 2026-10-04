//! Objects rendered onto a layout, each moved at its own updates.
//!
//! A render of a master — its objects panned on the room's cube onto
//! loudspeaker channels — has to land every update on its own sample: a
//! block rendered with the state at its start would move each object up to a
//! block late. So a block is split at every update any object makes inside
//! it, and each piece is rendered with the states in force over it. That is
//! the same walk whatever the render is for, a bed a writer codes or the
//! voices it gives an element of their own.

use hz_render::{Keyframe, Mixdown};

/// One object of a [`Following`]: the mixdown's source it is, its updates,
/// and the next one to act on.
struct Track {
    object: usize,
    keyframes: Vec<Keyframe>,
    next: usize,
}

/// A [`Mixdown`] whose objects follow their updates as it renders.
pub struct Following {
    mixdown: Mixdown,
    tracks: Vec<Track>,
}

impl Following {
    pub fn new(mixdown: Mixdown) -> Self {
        Self {
            mixdown,
            tracks: Vec::new(),
        }
    }

    /// The mixdown, to route speaker feeds and place still objects on.
    pub fn mixdown(&mut self) -> &mut Mixdown {
        &mut self.mixdown
    }

    /// Output channels a frame.
    pub fn channels(&self) -> usize {
        self.mixdown.channels()
    }

    /// Make the mixdown's input channel `channel` an object that follows
    /// `keyframes`.
    ///
    /// Heard from the first sample, where and as loud as its first update
    /// says — as every writer carries an object — rather than silent until
    /// that update arrives; a ramp the first update asks for has nothing to
    /// ramp from.
    pub fn object(&mut self, channel: usize, keyframes: &[Keyframe]) {
        let object = self.mixdown.object(channel);
        let mut keyframes = keyframes.to_vec();
        keyframes.sort_by_key(|k| k.sample_pos);
        if let Some(first) = keyframes.first() {
            self.mixdown.update(
                object,
                &Keyframe {
                    ramp_samples: 0,
                    ..*first
                },
            );
        }
        self.tracks.push(Track {
            object,
            keyframes,
            next: 0,
        });
    }

    /// Render `frames` frames of `input`, `stride` channels a frame, the
    /// first of them sample `at` of the programme, into `out`, as many
    /// channels a frame as the layout has. Split at every update an object
    /// makes inside them, so that each lands on its own sample.
    pub fn render(
        &mut self,
        input: &[f32],
        stride: usize,
        frames: usize,
        at: u64,
        out: &mut [f64],
    ) {
        let width = self.mixdown.channels();
        let mut done = 0;
        while done < frames {
            let now = at + done as u64;
            let mut next_update = u64::MAX;
            for track in &mut self.tracks {
                while let Some(state) = track.keyframes.get(track.next) {
                    if state.sample_pos > now {
                        next_update = next_update.min(state.sample_pos);
                        break;
                    }
                    self.mixdown.update(track.object, state);
                    track.next += 1;
                }
            }
            let end = next_update.saturating_sub(at).min(frames as u64) as usize;
            self.mixdown.render(
                &input[done * stride..frames * stride],
                stride,
                end - done,
                &mut out[done * width..frames * width],
            );
            done = end;
        }
    }
}
