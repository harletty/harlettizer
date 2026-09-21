//! Steadying the energies the placement is steered by.
//!
//! A block's energy is a mean square over 26.7 ms, and a steady sound's is
//! not steady: it ripples with where the block boundary falls in the
//! waveform. The fit is handed those energies as weights and answers a
//! slightly different question every block, and the flips — an element going
//! from something to nothing between two blocks — are partly that tremor
//! showing through. [`crate::anticipating`] handles one case of it, the
//! onset, by letting the placement see the next block's energy when that
//! block is much louder; nothing handles the rest.
//!
//! # Offline, so not causal
//!
//! A real-time encoder smooths causally: an attack it follows at once and a
//! release it lets decay. This encoder is offline and already reads a span
//! ahead of the one it folds, so its smoothing can be **centred on the
//! block**: a window reaching some blocks behind and some ahead, and the
//! placement steered by what the window says rather than by the block alone.
//!
//! Two ways of reading the window are written, and measured against each
//! other in `docs/clustering.md`:
//!
//! - [`Rule::Hold`]: the loudest the object is anywhere in the window. It
//!   rises before the attack and does not fall at the first dip, which is the
//!   quality wanted; and it is biased upwards for whichever objects ripple
//!   most, which is why the blocks ahead only count when they are louder by
//!   more than the ripple — the same guard [`crate::ARRIVING`] already is.
//! - [`Rule::Mean`]: the average over the window. Unbiased by the ripple,
//!   and it reaches an onset at a fraction of its weight rather than the
//!   whole of it.
//!
//! **Only the energy is smoothed.** The geometry a block is placed with is
//! its own, aligned with the audio: leading the geometry was measured at
//! twice the error, and the measurement is in `docs/clustering.md`.
//!
//! # What ships
//!
//! [`SMOOTHING`] is the rule in force, and it is what every fold uses — the
//! encoder's and the harness's — so that a report from one is a report about
//! the other. It is the anticipation that was there before with one more
//! block of reach: nothing behind, two blocks ahead, and a block ahead counted
//! only when it is at least twice as loud. What the alternatives measured,
//! and why the blocks behind are not taken, is in `docs/clustering.md`.

use crate::Object;
use std::collections::VecDeque;

/// How the window is read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rule {
    /// The loudest the object is in the window. The blocks behind count as
    /// they are; a block ahead counts only when it is at least `arriving`
    /// times the block's own energy, so that the ripple of a steady sound
    /// does not pass for an onset.
    Hold { arriving: f64 },
    /// The mean over the window.
    Mean,
}

/// A window of blocks around the one being placed, and how it is read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Smoothing {
    /// Blocks before this one that count.
    pub behind: usize,
    /// Blocks after this one that count — each of them a block of latency
    /// in the energy, which an offline encoder can afford.
    pub ahead: usize,
    pub rule: Rule,
}

/// The smoothing every fold in this project steers by.
///
/// A guarded hold two blocks ahead and none behind. The one-block version is
/// the anticipation as it was before there was a window to put it in; the
/// second block ahead is what a scene with onsets in it asked for, and it
/// costs nothing on the scenes without — see `docs/clustering.md` for the
/// tables, and [`crate::ARRIVING`] for the guard. The blocks behind bought
/// flips on every scene and cost error on the contested twelve-element folds,
/// and are not taken.
pub const SMOOTHING: Smoothing = Smoothing {
    behind: 0,
    ahead: 2,
    rule: Rule::Hold {
        arriving: crate::ARRIVING,
    },
};

/// The energies of the blocks around the one being placed, and the rule that
/// reads them.
///
/// Fed in two streams that may run apart: [`Smoother::record`] takes each
/// block's energies as it is read, oldest first, and [`Smoother::place`]
/// steers each block as it is folded, in the same order. The window a block
/// is placed by is whatever of it has been recorded — nothing behind at the
/// start of a stream, nothing ahead at its end — and a block that was never
/// recorded is placed by its own scene.
///
/// Holds no more than the window needs, and a block costs no allocation once
/// the stream is running.
#[derive(Debug)]
pub struct Smoother {
    smoothing: Smoothing,
    /// Energies of the recorded blocks the window can still reach, oldest
    /// first; `first` is the block number of the front.
    history: VecDeque<Vec<f64>>,
    first: u64,
    recorded: u64,
    placed: u64,
    /// Buffers taken out of the history, kept to be refilled.
    spare: Vec<Vec<f64>>,
}

impl Smoother {
    pub fn new(smoothing: Smoothing) -> Self {
        Self {
            smoothing,
            history: VecDeque::with_capacity(smoothing.behind + smoothing.ahead + 2),
            first: 0,
            recorded: 0,
            placed: 0,
            spare: Vec::new(),
        }
    }

    pub fn smoothing(&self) -> Smoothing {
        self.smoothing
    }

    /// How many blocks have been recorded so far.
    pub fn recorded(&self) -> u64 {
        self.recorded
    }

    /// How many blocks have been placed so far — the number of the next one
    /// to place.
    pub fn placed(&self) -> u64 {
        self.placed
    }

    /// The energies of the next block read: block [`Smoother::recorded`].
    pub fn record(&mut self, energies: &[f64]) {
        let mut slot = self.spare.pop().unwrap_or_default();
        slot.clear();
        slot.extend_from_slice(energies);
        self.history.push_back(slot);
        self.recorded += 1;
        self.trim();
    }

    /// Drop what no window will reach again: every block further behind the
    /// next one to place than the window reaches.
    fn trim(&mut self) {
        let keep_from = self.placed.saturating_sub(self.smoothing.behind as u64);
        while self.first < keep_from
            && let Some(old) = self.history.pop_front()
        {
            self.spare.push(old);
            self.first += 1;
        }
    }

    /// Steer the next block to place — block [`Smoother::placed`] — whose
    /// scene is `scene`: `out` is the same scene with each object's energy
    /// read from the window around it.
    ///
    /// The block's own energy is `scene`'s, whatever was recorded for it, so
    /// that a scene built with fresher gains than its recording wins.
    pub fn place(&mut self, scene: &[Object], out: &mut Vec<Object>) {
        let block = self.placed;
        self.placed += 1;
        out.clear();

        let Smoothing {
            behind,
            ahead,
            rule,
        } = self.smoothing;
        // The window, in block numbers, clipped to what was recorded.
        let from = block.saturating_sub(behind as u64).max(self.first);
        let to = (block + ahead as u64).min(self.recorded.saturating_sub(1));

        out.extend(scene.iter().enumerate().map(|(object, here)| {
            let own = here.energy;
            let energy = match rule {
                Rule::Hold { arriving } => {
                    let mut loudest = own;
                    for at in from..=to {
                        if at == block {
                            continue;
                        }
                        let then = self.energy_of(at, object);
                        if at < block || then >= own * arriving {
                            loudest = loudest.max(then);
                        }
                    }
                    loudest
                }
                Rule::Mean => {
                    let mut sum = 0.0;
                    let mut count = 0usize;
                    for at in from..=to {
                        sum += if at == block {
                            own
                        } else {
                            self.energy_of(at, object)
                        };
                        count += 1;
                    }
                    // A block that was never recorded has a window of nothing
                    // but itself.
                    if count == 0 { own } else { sum / count as f64 }
                }
            };
            Object { energy, ..*here }
        }));
        self.trim();
    }

    /// The recorded energy of `object` in block `at`, nought for an object the
    /// recording did not have.
    fn energy_of(&self, at: u64, object: usize) -> f64 {
        self.history
            .get((at - self.first) as usize)
            .and_then(|energies| energies.get(object))
            .copied()
            .unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(energies: &[f64]) -> Vec<Object> {
        energies
            .iter()
            .map(|energy| Object {
                position: [0.0, 1.0, 0.0],
                energy: *energy,
                size: 0.0,
                pinned: None,
                peak: 0.0,
                mode: hz_render::Mode::default(),
            })
            .collect()
    }

    fn energies(scene: &[Object]) -> Vec<f64> {
        scene.iter().map(|object| object.energy).collect()
    }

    /// A hold one block ahead is the anticipation as it was, block for block:
    /// counted only when it is twice as loud, and nothing behind. The rule
    /// that ships reaches one block further and is otherwise the same.
    #[test]
    fn a_hold_one_ahead_is_the_anticipation_as_it_was() {
        let blocks: Vec<Vec<f64>> = vec![
            vec![1.0, 1.0, 1e-9],
            vec![1.1, 0.9, 1e-9],
            vec![1.0, 0.1, 4.0],
            vec![0.9, 2.5, 4.0],
            vec![1.0, 1.0, 0.0],
        ];
        assert_eq!(
            SMOOTHING,
            Smoothing {
                behind: 0,
                ahead: 2,
                rule: Rule::Hold {
                    arriving: crate::ARRIVING
                }
            }
        );
        let mut smoother = Smoother::new(Smoothing {
            ahead: 1,
            ..SMOOTHING
        });
        let mut placing = Vec::new();
        let mut expected = Vec::new();
        for block in 0..blocks.len() {
            // What the encoder does: the block's own energies on the first
            // block, then the next block's as it is read.
            if smoother.recorded() == smoother.placed() {
                smoother.record(&blocks[block]);
            }
            if let Some(next) = blocks.get(block + 1) {
                smoother.record(next);
            }
            let here = scene(&blocks[block]);
            smoother.place(&here, &mut placing);
            let next: Vec<f64> = blocks.get(block + 1).cloned().unwrap_or_default();
            crate::anticipating(&here, &next, &mut expected);
            assert_eq!(
                energies(&placing),
                energies(&expected),
                "block {block} was placed differently from the anticipation"
            );
        }
    }

    /// A hold rises before an onset by as many blocks as it looks ahead, and
    /// stays up after the sound stops for as many as it looks behind.
    #[test]
    fn a_hold_rises_early_and_falls_late() {
        let on: Vec<f64> = (0..10)
            .map(|block| if (4..6).contains(&block) { 1.0 } else { 0.0 })
            .collect();
        let placed = run(
            Smoothing {
                behind: 2,
                ahead: 1,
                rule: Rule::Hold { arriving: 2.0 },
            },
            &on,
        );
        // Up from block 3 (one ahead of the onset at 4) through block 7 (two
        // behind the last loud block, 5).
        let expected: Vec<f64> = (0..10)
            .map(|block| if (3..=7).contains(&block) { 1.0 } else { 0.0 })
            .collect();
        assert_eq!(placed, expected);
    }

    /// The ahead side of a hold is guarded by the ripple: a block ahead that
    /// is louder by less than the guard does not count, and the ones behind
    /// count whatever they are.
    #[test]
    fn a_hold_ahead_is_guarded_and_behind_is_not() {
        let ripple = [1.0, 1.3, 1.0, 1.3, 1.0, 1.3];
        let placed = run(
            Smoothing {
                behind: 1,
                ahead: 1,
                rule: Rule::Hold { arriving: 2.0 },
            },
            &ripple,
        );
        // A quiet block between two louder ones takes the one behind it; a
        // loud block is its own.
        assert_eq!(placed, [1.0, 1.3, 1.3, 1.3, 1.3, 1.3]);
        let unguarded = run(
            Smoothing {
                behind: 0,
                ahead: 1,
                rule: Rule::Hold { arriving: 1.0 },
            },
            &ripple,
        );
        assert_eq!(unguarded, [1.3, 1.3, 1.3, 1.3, 1.3, 1.3]);
    }

    /// A mean is the mean of what the window can reach — shorter at both
    /// ends of a stream — and reaches an onset at a fraction of its weight.
    #[test]
    fn a_mean_is_over_what_the_window_reaches() {
        let on = [0.0, 0.0, 3.0, 3.0, 3.0, 0.0, 0.0];
        let placed = run(
            Smoothing {
                behind: 1,
                ahead: 1,
                rule: Rule::Mean,
            },
            &on,
        );
        assert_eq!(placed, [0.0, 1.0, 2.0, 3.0, 2.0, 1.0, 0.0]);
        // At the very start the window has nothing behind it, and it still
        // averages over what is there.
        let short = run(
            Smoothing {
                behind: 2,
                ahead: 0,
                rule: Rule::Mean,
            },
            &[4.0, 2.0, 0.0],
        );
        assert_eq!(short, [4.0, 3.0, 2.0]);
    }

    /// The block's own energy is the scene's, not the recording's, and a
    /// block nobody recorded is placed by its own scene alone.
    #[test]
    fn the_scene_is_authoritative_for_its_own_block() {
        let mut smoother = Smoother::new(Smoothing {
            behind: 1,
            ahead: 1,
            rule: Rule::Hold { arriving: 2.0 },
        });
        let mut placing = Vec::new();
        smoother.record(&[0.5]);
        smoother.record(&[0.5]);
        // Block 0, whose scene says something fresher than its recording.
        smoother.place(&scene(&[0.7]), &mut placing);
        assert_eq!(energies(&placing), [0.7]);
        // Block 1 as recorded, block 2 never recorded: the window behind
        // still counts, and the block is placed by its scene otherwise.
        smoother.place(&scene(&[0.5]), &mut placing);
        assert_eq!(energies(&placing), [0.5]);
        smoother.place(&scene(&[0.1]), &mut placing);
        assert_eq!(energies(&placing), [0.5]);
        smoother.place(&scene(&[0.1]), &mut placing);
        assert_eq!(energies(&placing), [0.1]);
    }

    /// Nothing is kept beyond what the window can reach, and the buffers are
    /// reused rather than reallocated.
    #[test]
    fn the_history_is_bounded() {
        let mut smoother = Smoother::new(Smoothing {
            behind: 2,
            ahead: 1,
            rule: Rule::Mean,
        });
        let mut placing = Vec::new();
        for block in 0..100u64 {
            smoother.record(&[block as f64]);
            if block >= 1 {
                smoother.place(&scene(&[(block - 1) as f64]), &mut placing);
            }
            assert!(
                smoother.history.len() <= 4,
                "block {block}: {} blocks kept",
                smoother.history.len()
            );
        }
        assert!(smoother.spare.len() <= 4);
    }

    /// One object at a time through a whole stream, the way the harness runs
    /// it: everything recorded up front, then placed block by block.
    fn run(smoothing: Smoothing, blocks: &[f64]) -> Vec<f64> {
        let mut smoother = Smoother::new(smoothing);
        let mut placing = Vec::new();
        let mut out = Vec::new();
        let mut recorded = 0usize;
        for (block, energy) in blocks.iter().enumerate() {
            while recorded <= block + smoothing.ahead && recorded < blocks.len() {
                smoother.record(&[blocks[recorded]]);
                recorded += 1;
            }
            smoother.place(&scene(&[*energy]), &mut placing);
            out.push(placing[0].energy);
        }
        out
    }
}
