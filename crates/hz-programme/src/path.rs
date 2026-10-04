//! An object's path through the room, from a master's updates.
//!
//! A master moves an object by updates: be here, this loud, and take this
//! long getting there. Between updates it holds; across a ramp it moves in a
//! straight line through the cube. So an object's path is piecewise linear,
//! and is worked out once from the updates, whatever a writer then does with
//! it — cut a format's position blocks from it, or put the gain it carries
//! on the object's samples.

use hz_render::Keyframe;

/// Where an object is and how loud, at one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct State {
    pub position: [f64; 3],
    pub gain: f64,
}

impl State {
    pub fn lerp(self, to: State, a: f64) -> State {
        State {
            position: [0, 1, 2].map(|i| self.position[i] + (to.position[i] - self.position[i]) * a),
            gain: self.gain + (to.gain - self.gain) * a,
        }
    }
}

/// A stretch of an object's path: a straight line from `from` at `start` to
/// `to` at `end`, which is a standstill when they agree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub start: u64,
    /// `u64::MAX` for the last, which lasts.
    pub end: u64,
    pub from: State,
    pub to: State,
}

impl Segment {
    pub fn at(&self, t: u64) -> State {
        if self.end == u64::MAX || self.from == self.to {
            return self.from;
        }
        let a = (t.saturating_sub(self.start)) as f64 / (self.end - self.start) as f64;
        self.from.lerp(self.to, a.min(1.0))
    }

    pub fn still(&self) -> bool {
        self.from.position == self.to.position
    }
}

/// An object's path through the programme, from the master's updates.
#[derive(Debug, Clone)]
pub struct Path {
    segments: Vec<Segment>,
}

impl Path {
    /// The updates as a path: each one moves the object from wherever it is
    /// when the update arrives — part way along a ramp the update cuts short,
    /// if it does — to where it asks, over the samples it asks for.
    ///
    /// Before its first update it is where and as loud as that update says,
    /// which is what `encode` seeds an element with: a master whose first
    /// update comes a few samples in still has audio before it, and a path
    /// that was silent until then dropped those samples from every object —
    /// near-silence on the masters seen, 171 samples of it, but not the
    /// master's.
    pub fn new(keyframes: &[Keyframe]) -> Self {
        let mut keyframes = keyframes.to_vec();
        keyframes.sort_by_key(|k| k.sample_pos);
        let mut from = keyframes.first().map_or(
            State {
                position: [0.0, 1.0, 0.0],
                gain: 0.0,
            },
            |k| State {
                position: k.position,
                gain: k.gain,
            },
        );
        let mut to = from;
        let (mut ramp_start, mut ramp_end) = (0u64, 0u64);
        let mut last = 0u64;
        let mut segments = Vec::new();
        let value = |from: State, to: State, start: u64, end: u64, t: u64| {
            if t >= end || end == start {
                to
            } else {
                from.lerp(to, (t - start) as f64 / (end - start) as f64)
            }
        };
        let close = |segments: &mut Vec<Segment>,
                     from: State,
                     to: State,
                     start: u64,
                     end: u64,
                     last: u64,
                     t: u64| {
            // The ramp's share of [last, t), then the hold after it.
            if end > last {
                let stop = end.min(t);
                if stop > last {
                    segments.push(Segment {
                        start: last,
                        end: stop,
                        from: value(from, to, start, end, last),
                        to: value(from, to, start, end, stop),
                    });
                }
            }
            let hold = end.max(last);
            if t > hold {
                let at = value(from, to, start, end, hold);
                segments.push(Segment {
                    start: hold,
                    end: t,
                    from: at,
                    to: at,
                });
            }
        };
        for keyframe in &keyframes {
            let t = keyframe.sample_pos;
            close(&mut segments, from, to, ramp_start, ramp_end, last, t);
            let now = value(from, to, ramp_start, ramp_end, t);
            let target = State {
                position: keyframe.position,
                gain: keyframe.gain,
            };
            from = if keyframe.ramp_samples == 0 {
                target
            } else {
                now
            };
            to = target;
            ramp_start = t;
            ramp_end = t + u64::from(keyframe.ramp_samples);
            last = last.max(t);
        }
        close(
            &mut segments,
            from,
            to,
            ramp_start,
            ramp_end,
            last,
            ramp_end.max(last),
        );
        segments.push(Segment {
            start: ramp_end.max(last),
            end: u64::MAX,
            from: to,
            to,
        });
        segments.retain(|s| s.end > s.start);
        Self { segments }
    }

    /// The segment holding sample `t`, searched from `hint` onwards: the
    /// callers walk forwards.
    pub fn segment(&self, t: u64, hint: &mut usize) -> &Segment {
        if self.segments[*hint].start > t {
            *hint = 0;
        }
        while self.segments[*hint].end <= t {
            *hint += 1;
        }
        &self.segments[*hint]
    }

    /// Where the object starts.
    pub fn origin(&self) -> [f64; 3] {
        self.segments[0].from.position
    }

    /// The gain at sample `t`.
    pub fn gain(&self, t: u64, hint: &mut usize) -> f64 {
        self.segment(t, hint).at(t).gain
    }

    /// When the object, standing at `position` at sample `t`, next moves:
    /// `None` if it never does.
    pub fn still_until(&self, t: i64, position: [f64; 3], hint: &mut usize) -> Option<u64> {
        self.segment(t.max(0) as u64, hint);
        let mut i = *hint;
        while let Some(segment) = self.segments.get(i) {
            if !segment.still() || segment.from.position != position {
                return Some(segment.start);
            }
            if segment.end == u64::MAX {
                return None;
            }
            i += 1;
        }
        None
    }
}

/// Move `state` to the latest of `keyframes` in force at sample `at`,
/// walking on from `next`: the update a writer places an element on — its
/// target, not part way along its ramp. Returns whether any update was
/// taken, which is what decides that something moved.
///
/// The callers walk forwards, so this is one comparison an element once
/// nothing is due.
pub fn advance(keyframes: &[Keyframe], next: &mut usize, state: &mut Keyframe, at: u64) -> bool {
    let mut moved = false;
    while let Some(keyframe) = keyframes.get(*next) {
        if keyframe.sample_pos > at {
            break;
        }
        *state = *keyframe;
        *next += 1;
        moved = true;
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyframe(sample_pos: u64, x: f64, gain: f64, ramp_samples: u32) -> Keyframe {
        Keyframe {
            sample_pos,
            position: [x, 1.0, 0.0],
            gain,
            ramp_samples,
            ..Keyframe::default()
        }
    }

    fn x_at(path: &Path, t: u64) -> f64 {
        path.segment(t, &mut 0).at(t).position[0]
    }

    /// The update in force is the latest one due, and nothing moves until
    /// the next is.
    #[test]
    fn the_update_in_force_is_the_latest_due() {
        let keyframes = [keyframe(0, -1.0, 1.0, 0), keyframe(100, 1.0, 1.0, 0)];
        let (mut next, mut state) = (0, Keyframe::default());
        assert!(advance(&keyframes, &mut next, &mut state, 99));
        assert_eq!(state, keyframes[0]);
        assert!(!advance(&keyframes, &mut next, &mut state, 99));
        assert!(advance(&keyframes, &mut next, &mut state, 1000));
        assert_eq!((next, state), (2, keyframes[1]));
    }

    /// Holds, ramps, a ramp cut short by the next update, and a jump.
    #[test]
    fn a_path_follows_the_updates() {
        let path = Path::new(&[
            keyframe(0, -1.0, 1.0, 0),
            keyframe(100, 1.0, 1.0, 200),
            // Cuts the ramp at its midpoint, x = 0, and heads back.
            keyframe(200, -1.0, 0.5, 100),
            keyframe(400, 0.5, 1.0, 0),
        ]);
        assert_eq!(x_at(&path, 50), -1.0);
        assert!((x_at(&path, 150) + 0.5).abs() < 1e-12);
        assert!(x_at(&path, 200).abs() < 1e-12);
        assert!((x_at(&path, 250) + 0.5).abs() < 1e-12);
        assert_eq!(x_at(&path, 300), -1.0);
        assert_eq!(x_at(&path, 399), -1.0);
        assert_eq!(x_at(&path, 400), 0.5);
        assert_eq!(x_at(&path, 1_000_000), 0.5);
        let gain = |t| path.segment(t, &mut 0).at(t).gain;
        assert!((gain(250) - 0.75).abs() < 1e-12);
    }

    /// Before its first update an object is where and as loud as that update
    /// says — its audio from the first sample, as `encode` carries it — and
    /// a ramp the first update asks for has nothing to ramp from.
    #[test]
    fn an_object_plays_from_the_first_sample() {
        let path = Path::new(&[keyframe(171, 0.3, 0.5, 0)]);
        assert_eq!(path.segment(0, &mut 0).at(0).gain, 0.5);
        assert_eq!(path.segment(171, &mut 0).at(171).gain, 0.5);
        assert_eq!(path.origin(), [0.3, 1.0, 0.0]);
        let ramped = Path::new(&[keyframe(171, 0.3, 0.5, 1000)]);
        assert_eq!(ramped.segment(500, &mut 0).at(500).gain, 0.5);
        assert_eq!(ramped.still_until(0, [0.3, 1.0, 0.0], &mut 0), None);
    }
}
