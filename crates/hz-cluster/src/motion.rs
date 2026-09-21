//! What of an element's movement a listener would notice.
//!
//! Every other figure this crate reports is a **static** error: how far
//! `Σ_k w_ik·r(q_k)` is from `r(p_i)` at one instant. The metric has no time
//! axis, so a fold that is right at every instant and *different* at every
//! instant scores perfectly — and that is a fold whose elements never sit
//! still, which is the one defect a listener has reported and the one the
//! figures could not see. This is the ruler that can see it. It is the tenth
//! lever of `docs/clustering-next.md`, and it comes first because without it
//! there is no way to tell a rule that fixes the defect from a rule that does
//! nothing.
//!
//! # Two things that are not the same
//!
//! An element that crosses the room because the scene crossed the room is the
//! fold working. An element that leaves its place and comes back is the fold
//! failing. A count of flips cannot tell them apart, and neither can a mean
//! travel: both charge the same for either. So this measures them separately,
//! over a window of the length the ear takes to make a *movement* out of a
//! change of place:
//!
//! - **the net** — how far the element ended up from where it started, which
//!   is movement the mix asked for;
//! - **the wobble** — the rest of the path, which went nowhere.
//!
//! For an element that walks steadily the path is the net and the wobble is
//! nought. For one that darts out by `θ` and comes back the net is nought and
//! the wobble is `2θ`, so the excursion the listener heard is half of it.
//!
//! # What makes either of them audible
//!
//! Two published quantities, and only two.
//!
//! **The blur.** A direction is not heard to a degree. Localisation blur is
//! about three and a half degrees in front, ten at the sides and five and a
//! half behind [`Hearing`] — widest at the sides, and wider behind than in
//! front. A change smaller than that is not a change of place at all.
//!
//! **The window.** A change of place is heard as *motion* only if it lasts:
//! below a minimum integration time of about two tenths of a second the ear
//! does not build a movement percept out of it, and what a shorter change
//! gives is an unsteady image rather than a moving source. That is also why
//! the minimum audible movement angle grows with velocity — at a fixed
//! integration time, a faster source has to cover more ground before the ear
//! has looked long enough.
//!
//! Those two together are the whole rule, and they collapse into something
//! simpler than they look: **over a window of exactly the integration time,
//! the velocity term is spent, and what is left is the blur.** A movement is
//! noticed when the net exceeds the blur; a wobble is noticed when the
//! excursion — half the wobble — exceeds it. Taking a window shorter than the
//! integration time is what would bring the velocity back, and there is no
//! reason to: the encoder is offline and the window is free.
//!
//! # Where these numbers come from, and why that is a departure
//!
//! Rule 1 of `docs/clustering-next.md` is that no constant comes from
//! anywhere but a measurement of ours. These four are the exception, and the
//! exception is stated rather than hidden: they are properties of a
//! **listener**, not of a fold, so `cargo xtask cluster` cannot produce them
//! and no amount of running it would. They are read from the published
//! literature — see `docs/clustering-next.md` references 29 to 31 — and what
//! would calibrate them *here*, on this repository's own panner and
//! presentations, is the listening bench of lever 9, which has scored no
//! sheet yet. Until it has, every figure this module reports is a figure
//! against a published threshold and not against this project's ears, and
//! that is the honest reading of it.
//!
//! # Windows a wobble straddles
//!
//! The windows overlap — one a block — so an excursion is seen whole by most
//! of the windows it falls in and by halves at its edges. A window that
//! catches only the leaving, or only the returning, sees an element that went
//! somewhere and reports movement, because over that window it did. So a
//! wobbling fold reads as *some* movement as well, and the two figures are
//! not a partition of the windows. The wobble is the figure this exists for
//! and it is not diluted by the effect; the movement figure is context, and
//! it reads a little high on a fold that wobbles.
//!
//! # What it does not model
//!
//! Elevation. The blur above is the horizontal plane's, which is what the
//! sources measured, and an element that wobbles overhead is charged the blur
//! of its azimuth. That is a floor on the charge rather than a fair one — the
//! blur is wider out of the horizontal plane, so a vertical wobble is
//! over-charged — and it is written down here rather than quietly folded in.
//! It is also the smaller half of the problem: the defect this exists for was
//! reported at the rear, not overhead.

use std::collections::VecDeque;

/// What a listener notices, in degrees and in seconds.
///
/// Not measured here — see the module's note on where these come from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hearing {
    /// Localisation blur straight ahead, in degrees.
    pub ahead: f64,
    /// At the side, where it is widest.
    pub aside: f64,
    /// And behind, which is wider than in front and narrower than the side.
    pub behind: f64,
    /// How long the ear takes to make a movement out of a change of place, in
    /// seconds. Also the window every figure here is taken over.
    pub window: f64,
}

/// The listener every figure in this module is against.
///
/// Blur for speech, which is what a programme mostly is: 3.6° ahead, 10° at
/// the side, 5.5° behind. The window is the minimum integration time for a
/// movement percept, two tenths of a second.
pub const HEARING: Hearing = Hearing {
    ahead: 3.6,
    aside: 10.0,
    behind: 5.5,
    window: 0.2,
};

impl Default for Hearing {
    fn default() -> Self {
        HEARING
    }
}

impl Hearing {
    /// The blur at a direction: the smallest change of place heard there.
    ///
    /// Piecewise linear in the azimuth, `ahead` to `aside` over the first
    /// quarter turn and `aside` to `behind` over the second. The elevation is
    /// dropped — see the module's note.
    pub fn blur(&self, direction: [f64; 3]) -> f64 {
        // The master's frame: x to the right, y to the front, z up. Azimuth
        // from straight ahead, folded onto a half turn since left and right
        // hear alike.
        let azimuth = direction[0].atan2(direction[1]).abs().to_degrees();
        if azimuth <= 90.0 {
            let along = azimuth / 90.0;
            self.ahead + (self.aside - self.ahead) * along
        } else {
            let along = (azimuth - 90.0) / 90.0;
            self.aside + (self.behind - self.aside) * along
        }
    }

    /// How many blocks of `seconds_a_block` the window reaches over, at least
    /// two so that there is a step in it at all.
    pub fn span(&self, seconds_a_block: f64) -> usize {
        if seconds_a_block <= 0.0 {
            return 2;
        }
        ((self.window / seconds_a_block).round() as usize).max(2)
    }
}

/// What one element did over one window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    /// How far it ended up from where it started, in degrees.
    pub net: f64,
    /// The rest of the path — twice the excursion of an out-and-back.
    pub wobble: f64,
    /// The blur where it spent the window.
    pub blur: f64,
    /// The most it carried at any point in the window, as a share of the
    /// loudest element there.
    pub carried: f64,
}

impl Window {
    /// Whether the element went somewhere a listener would hear it go.
    pub fn moved(&self) -> bool {
        self.net > self.blur
    }

    /// Whether it left its place and came back by more than the blur, which
    /// is nothing any mix asked for.
    pub fn wobbled(&self) -> bool {
        self.wobble * 0.5 > self.blur
    }

    /// By how many degrees the excursion passed the blur, or nought.
    pub fn over(&self) -> f64 {
        (self.wobble * 0.5 - self.blur).max(0.0)
    }
}

/// What a fold's elements did, window by window.
///
/// Fed one block at a time in order, oldest first. Holds the window and
/// nothing more, and a block costs no allocation once the stream is running.
#[derive(Debug)]
pub struct Motion {
    hearing: Hearing,
    span: usize,
    /// Each recorded block's element directions and what each carried.
    history: VecDeque<(Vec<[f64; 3]>, Vec<f64>)>,
    spare: Vec<(Vec<[f64; 3]>, Vec<f64>)>,
    /// Windows examined, and the ones where something was noticed.
    windows: u64,
    wobbled: u64,
    moved: u64,
    /// Degrees past the blur, summed over the windows that wobbled, and the
    /// worst one.
    over: f64,
    worst: f64,
    /// The share of what was playing that sat in an element wobbling, summed
    /// over the windows, and the total share examined.
    wobbling_share: f64,
    share: f64,
    /// Degrees of path that went nowhere, and degrees that went somewhere,
    /// each weighted by what the element carried. These have no threshold in
    /// them — see [`Motion::invented`].
    weighted_wobble: f64,
    weighted_net: f64,
    /// How long a window really is, in seconds — see [`Motion::new`].
    window: f64,
}

impl Motion {
    pub fn new(hearing: Hearing, seconds_a_block: f64) -> Self {
        let span = hearing.span(seconds_a_block);
        Self {
            hearing,
            span,
            // The window a rate is divided by is the one the windows are
            // actually taken over, which is a whole number of blocks and so
            // almost never the ear's own figure. At the encoder's block it is
            // eight of them, 0.213 s against the 0.200 s `HEARING` asks for —
            // and dividing by the asked-for figure reported every rate about
            // seven per cent high, and made "the same figures from either
            // side" true only where the harness and the encoder happened to
            // use the same block.
            window: span as f64 * seconds_a_block,
            history: VecDeque::with_capacity(span + 2),
            spare: Vec::new(),
            windows: 0,
            wobbled: 0,
            moved: 0,
            over: 0.0,
            worst: 0.0,
            wobbling_share: 0.0,
            share: 0.0,
            weighted_wobble: 0.0,
            weighted_net: 0.0,
        }
    }

    /// How many blocks the window reaches over.
    pub fn span(&self) -> usize {
        self.span
    }

    /// Take one block: where each element ended up, and what each carried.
    ///
    /// `carried` is in the caller's own units; only ratios within a block are
    /// used, so it may be energy, power or anything proportional to either.
    pub fn record(&mut self, directions: &[[f64; 3]], carried: &[f64]) {
        let mut slot = self.spare.pop().unwrap_or_default();
        slot.0.clear();
        slot.0.extend_from_slice(directions);
        slot.1.clear();
        slot.1.extend_from_slice(carried);
        self.history.push_back(slot);
        if self.history.len() > self.span + 1 {
            if let Some(old) = self.history.pop_front() {
                self.spare.push(old);
            }
        }
        if self.history.len() == self.span + 1 {
            self.close();
        }
    }

    /// The window that just filled, element by element.
    fn close(&mut self) {
        let elements = self
            .history
            .iter()
            .map(|(directions, _)| directions.len())
            .min()
            .unwrap_or(0);
        for element in 0..elements {
            let Some(window) = self.window(element) else {
                continue;
            };
            self.windows += 1;
            self.share += window.carried;
            self.weighted_wobble += window.carried * window.wobble;
            self.weighted_net += window.carried * window.net;
            if window.moved() {
                self.moved += 1;
            }
            if window.wobbled() {
                self.wobbled += 1;
                self.over += window.over();
                self.worst = self.worst.max(window.over());
                self.wobbling_share += window.carried;
            }
        }
    }

    /// One element over the window the history holds, or `None` when nobody
    /// was listening to it: an element carrying nothing is free to be
    /// anywhere, which is what `crate::separate` is for, and charging it for
    /// moving would report a feature as a fault.
    fn window(&self, element: usize) -> Option<Window> {
        let mut carried = 0.0f64;
        for (_, energies) in &self.history {
            let loudest = energies.iter().fold(0.0f64, |m, e| m.max(*e));
            if loudest <= 0.0 {
                continue;
            }
            let share = energies.get(element).copied().unwrap_or(0.0) / loudest;
            carried = carried.max(share);
        }
        if carried <= crate::metric::AUDIBLE_FLOOR {
            return None;
        }

        let mut path = 0.0;
        let mut summed = [0.0f64; 3];
        let mut previous: Option<[f64; 3]> = None;
        for (directions, _) in &self.history {
            let at = *directions.get(element)?;
            if let Some(before) = previous {
                path += apart(before, at);
            }
            for axis in 0..3 {
                summed[axis] += at[axis];
            }
            previous = Some(at);
        }
        let first = *self.history.front()?.0.get(element)?;
        let last = *self.history.back()?.0.get(element)?;
        let net = apart(first, last);
        // The blur where it spent the window, not where it ended: an
        // excursion that starts behind and pokes forward is judged by the
        // place it belongs to.
        let length = (summed[0] * summed[0] + summed[1] * summed[1] + summed[2] * summed[2]).sqrt();
        let where_it_was = if length > 1e-12 {
            [summed[0] / length, summed[1] / length, summed[2] / length]
        } else {
            last
        };
        Some(Window {
            net,
            wobble: (path - net).max(0.0),
            blur: self.hearing.blur(where_it_was),
            carried,
        })
    }

    /// Windows examined — one per element per block, once the first window
    /// has filled.
    pub fn windows(&self) -> u64 {
        self.windows
    }

    /// The share of them where the element left its place and came back by
    /// more than the blur.
    pub fn wobbling(&self) -> f64 {
        if self.windows == 0 {
            return 0.0;
        }
        self.wobbled as f64 / self.windows as f64
    }

    /// And the share where it went somewhere, which is the fold following the
    /// scene and not a fault.
    pub fn moving(&self) -> f64 {
        if self.windows == 0 {
            return 0.0;
        }
        self.moved as f64 / self.windows as f64
    }

    /// Degrees past the blur, averaged over the windows that wobbled.
    pub fn over(&self) -> f64 {
        if self.wobbled == 0 {
            return 0.0;
        }
        self.over / self.wobbled as f64
    }

    /// And the worst single window.
    pub fn worst(&self) -> f64 {
        self.worst
    }

    /// How much of what was playing sat in an element that wobbled, as a
    /// share of what was playing at all.
    pub fn wobbling_energy(&self) -> f64 {
        if self.share <= 0.0 {
            return 0.0;
        }
        self.wobbling_share / self.share
    }
}

impl Motion {
    /// Movement the mix never asked for, in degrees a second, weighted by
    /// what each element carries.
    ///
    /// # Why there is a figure with no threshold in it
    ///
    /// [`Motion::wobbling`] asks whether one excursion, on its own, passes
    /// the blur. Measured on the fold a listener complained about, it does
    /// not: the excursions there are about three degrees and the blur behind
    /// is five and a half, so **every one of them is individually below what
    /// the published threshold says is a change of place at all** — and the
    /// figure reads nought on the very fold that was reported.
    ///
    /// That is a result and not a defect of the ruler, and what it says is
    /// that the thing being heard is not one excursion. It is a *sustained*
    /// fluctuation: an element that never settles, leaving and returning
    /// every few blocks for as long as it carries anything. The blur is the
    /// threshold for telling two places apart once; it is not the threshold
    /// for noticing that a place will not hold still, and the second is the
    /// smaller of the two by an amount no source in
    /// `docs/clustering-next.md` puts a number on.
    ///
    /// So this is the figure without a threshold: how many degrees a second
    /// of path go nowhere. It cannot say "audible" and does not claim to.
    /// What it can do is rank two folds of the same programme, which is what
    /// a lever needs, and put a number on the gap to a delivered stream.
    /// Turning it into a threshold is what lever 9's listening is for.
    pub fn invented(&self) -> f64 {
        if self.share <= 0.0 || self.window <= 0.0 {
            return 0.0;
        }
        self.weighted_wobble / self.share / self.window
    }

    /// And movement it did ask for, on the same terms.
    pub fn asked(&self) -> f64 {
        if self.share <= 0.0 || self.window <= 0.0 {
            return 0.0;
        }
        self.weighted_net / self.share / self.window
    }

    /// How long a window really is, in seconds: a whole number of blocks, and
    /// so not usually [`Hearing::window`] itself.
    pub fn window_seconds(&self) -> f64 {
        self.window
    }
}

/// The angle between two directions, in degrees.
///
/// # The one place this ruler is the wrong shape
///
/// Both arguments are **cube positions read from the room's centre**, so the
/// angle is what a listener sitting at the middle would turn their head by.
/// That is the right question almost everywhere and the wrong one near the
/// centre itself, where a position has almost no length and the smallest step
/// the wire can code swings it a long way: one step is about two degrees out
/// at the wall and about **eighteen** at a tenth of the way out, so an element
/// parked near the middle can be reported as wobbling hugely while moving by
/// the least the format can express.
///
/// Nothing here corrects for it, because a correction would be a second rule
/// with a radius in it and there is no measurement to set one by. What keeps
/// it from mattering in practice is that an element near the centre is an
/// element the scene put nowhere in particular, and the energy weighting
/// already discounts what nobody is listening to — but a figure taken on a
/// scene with elements parked at the origin should be read knowing this.
fn apart(a: [f64; 3], b: [f64; 3]) -> f64 {
    let dot = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let length = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
        * (b[0] * b[0] + b[1] * b[1] + b[2] * b[2]).sqrt();
    if length <= 1e-12 {
        return 0.0;
    }
    (dot / length).clamp(-1.0, 1.0).acos().to_degrees()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A direction at an azimuth, degrees from straight ahead.
    fn at(degrees: f64) -> [f64; 3] {
        let angle = degrees.to_radians();
        [angle.sin(), angle.cos(), 0.0]
    }

    /// The blur is widest at the side, and behind is between the two.
    #[test]
    fn the_blur_is_widest_at_the_side() {
        let hearing = HEARING;
        let ahead = hearing.blur(at(0.0));
        let aside = hearing.blur(at(90.0));
        let behind = hearing.blur(at(180.0));
        assert!((ahead - HEARING.ahead).abs() < 1e-9, "{ahead}");
        assert!((aside - HEARING.aside).abs() < 1e-9, "{aside}");
        assert!((behind - HEARING.behind).abs() < 1e-9, "{behind}");
        assert!(ahead < behind && behind < aside);
        // Left and right hear alike.
        assert!((hearing.blur(at(-40.0)) - hearing.blur(at(40.0))).abs() < 1e-9);
    }

    /// The window is the ear's, in blocks of whatever the encoder folds.
    #[test]
    fn the_window_is_the_ears_and_not_the_encoders() {
        // 1280 samples at 48 kHz is 26.7 ms, so two tenths of a second is
        // seven and a half blocks.
        assert_eq!(HEARING.span(1280.0 / 48_000.0), 8);
        // And never less than a step.
        assert_eq!(HEARING.span(10.0), 2);
    }

    /// An element following a scene across the room is the fold working, and
    /// is not charged for it.
    #[test]
    fn a_scene_that_turns_is_not_a_wobble() {
        let mut motion = Motion::new(HEARING, 1280.0 / 48_000.0);
        for block in 0..40 {
            // Three degrees a block, which over the window is 24° — well past
            // any blur, so it is movement and it is meant to be.
            motion.record(&[at(3.0 * block as f64)], &[1.0]);
        }
        assert!(motion.windows() > 0);
        assert_eq!(motion.wobbling(), 0.0, "a steady turn wobbled");
        assert!(
            motion.moving() > 0.9,
            "a steady turn did not read as movement: {}",
            motion.moving()
        );
    }

    /// An element that stays where it is is neither.
    #[test]
    fn an_element_that_stays_put_is_neither() {
        let mut motion = Motion::new(HEARING, 1280.0 / 48_000.0);
        for _ in 0..40 {
            motion.record(&[at(150.0)], &[1.0]);
        }
        assert_eq!(motion.wobbling(), 0.0);
        assert_eq!(motion.moving(), 0.0);
    }

    /// The scene this ruler exists for: an element at rest that darts away
    /// and comes back inside the window.
    #[test]
    fn an_element_that_darts_out_and_back_is_noticed() {
        let mut motion = Motion::new(HEARING, 1280.0 / 48_000.0);
        // At the rear quarter, where the blur is 7.75°, so a 12° excursion is
        // past it and the same excursion in front would not be judged the
        // same way.
        let home = at(135.0);
        let away = at(147.0);
        for block in 0..40 {
            let where_it_is = if block % 16 == 8 || block % 16 == 9 {
                away
            } else {
                home
            };
            motion.record(&[where_it_is], &[1.0]);
        }
        assert!(
            motion.wobbling() > 0.0,
            "the excursion was not noticed at all"
        );
        assert!(
            motion.over() > 0.0 && motion.worst() >= motion.over(),
            "over {} worst {}",
            motion.over(),
            motion.worst()
        );
        // Most windows see the whole excursion and read it for what it is.
        // The ones that catch only its leading or trailing edge see an
        // element that went somewhere and had not come back yet, and read
        // that as movement — see the note on straddling windows.
        assert!(
            motion.wobbling() > motion.moving(),
            "wobbling {} against moving {}",
            motion.wobbling(),
            motion.moving()
        );
    }

    /// The same excursion is forgiven at the side and not in front, which is
    /// the whole reason the blur is not one number.
    #[test]
    fn the_same_excursion_is_judged_by_where_it_happens() {
        let excursion = |home: f64, away: f64| {
            let mut motion = Motion::new(HEARING, 1280.0 / 48_000.0);
            for block in 0..40 {
                let where_it_is = if block % 16 == 8 { at(away) } else { at(home) };
                motion.record(&[where_it_is], &[1.0]);
            }
            motion.wobbling()
        };
        // Five degrees: past the blur in front, inside it at the side.
        assert!(excursion(0.0, 5.0) > 0.0, "5° in front was forgiven");
        assert_eq!(excursion(90.0, 95.0), 0.0, "5° at the side was charged");
    }

    /// The figure with no threshold ranks a fold that wobbles above one that
    /// does not, where the threshold figure cannot see either.
    #[test]
    fn the_rate_ranks_what_the_threshold_cannot_see() {
        let run = |excursion: f64| {
            let mut motion = Motion::new(HEARING, 1280.0 / 48_000.0);
            for block in 0..80 {
                let where_it_is = if block % 4 < 2 {
                    at(120.0)
                } else {
                    at(120.0 + excursion)
                };
                motion.record(&[where_it_is], &[1.0]);
            }
            motion
        };
        // Two degrees at the rear quarter, well inside a blur of 7.75°.
        let small = run(2.0);
        let large = run(3.5);
        assert_eq!(small.wobbling(), 0.0, "the threshold fired on 2°");
        assert_eq!(large.wobbling(), 0.0, "the threshold fired on 3.5°");
        assert!(
            large.invented() > small.invented() * 1.5,
            "the rate did not rank them: {} against {}",
            large.invented(),
            small.invented()
        );
        // A steady turn invents nothing, however far it goes.
        let mut steady = Motion::new(HEARING, 1280.0 / 48_000.0);
        for block in 0..80 {
            steady.record(&[at(3.0 * block as f64)], &[1.0]);
        }
        assert!(
            steady.invented() < 0.01 && steady.asked() > 50.0,
            "a steady turn: invented {} asked {}",
            steady.invented(),
            steady.asked()
        );
    }

    /// An element nobody is listening to is free to be anywhere.
    #[test]
    fn an_element_carrying_nothing_is_not_charged_for_moving() {
        let mut motion = Motion::new(HEARING, 1280.0 / 48_000.0);
        for block in 0..40 {
            let wanderer = at(if block % 4 == 0 { 0.0 } else { 120.0 });
            motion.record(&[at(30.0), wanderer], &[1.0, 0.0]);
        }
        assert_eq!(motion.wobbling(), 0.0, "a parked element was charged");
    }

    /// What is playing in the elements that wobble, which is the figure that
    /// says whether the defect is in the mix or in a corner of it.
    #[test]
    fn the_share_of_what_is_playing_is_reported() {
        let mut motion = Motion::new(HEARING, 1280.0 / 48_000.0);
        for block in 0..40 {
            let wobbler = at(if block % 16 == 8 { 20.0 } else { 0.0 });
            // The steady one carries ten times what the wobbling one does.
            motion.record(&[at(120.0), wobbler], &[1.0, 0.1]);
        }
        let share = motion.wobbling_energy();
        assert!(
            share > 0.0 && share < 0.2,
            "a tenth of the scene wobbling reported as {share}"
        );
    }
}
