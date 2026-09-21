//! What an object has to carry to count at all.
//!
//! The metric's floor was forty decibels under the loudest object in the
//! block, and nothing else. Being relative, it promotes noise in a quiet
//! block — the loudest thing in a block of nothing is still the loudest —
//! and hides a real object in a loud one, where a voice forty decibels under
//! an explosion is still a voice. What decides whether an object can be heard
//! is not its neighbour but the level it plays at.
//!
//! # From the stream to the room
//!
//! The playback level is known, in three steps that are each somebody's
//! standard. A programme states its `dialnorm`, the level of its dialogue in
//! decibels below full scale, and a decoder applies the gain that brings it
//! to the reference, −31 (ATSC A/52). A room is calibrated so that pink
//! noise at −20 dBFS in one channel reads 85 dB SPL (SMPTE RP 200), which
//! puts full scale at 105 dB. And the threshold of hearing in quiet at one
//! kilohertz is 2.4 dB SPL (ISO 226). So an object's power in the stream is
//! inaudible below
//!
//! ```text
//! threshold − (105 + (−31 − dialnorm))  decibels below full scale
//! ```
//!
//! which is −102.6 dBFS for a programme at the reference and rises by a
//! decibel for every decibel the decoder turns it down. Below it an object
//! does not seed an element, does not pull a centre, does not set a worst
//! case, and is not counted. The relative floor stays as a second guard, for
//! the object that is audible in the room and not beside its neighbour.
//!
//! One threshold for every band, taken at one kilohertz, since the plain
//! power the fold weighs by has no bands. The perceptual rule's bands could
//! each have ISO 226's own threshold; that is the per-band floor the lever
//! names, and it is not written until the rule that would use it ships.

/// Where a room plays full scale, in decibels of sound pressure: pink noise
/// at −20 dBFS in one channel reading 85 dB SPL, SMPTE RP 200.
pub const FULL_SCALE_SPL: f64 = 105.0;

/// The threshold of hearing in quiet at one kilohertz, ISO 226:2003.
pub const THRESHOLD_SPL: f64 = 2.4;

/// The `dialnorm` at which a decoder applies no gain, ATSC A/52.
pub const DIALNORM_REFERENCE: f64 = -31.0;

/// The relative guard: how far below the loudest object in the block
/// something may sit before it stops counting, as a fraction of power —
/// forty decibels. See [`crate::metric::AUDIBLE_FLOOR`].
pub const RELATIVE: f64 = crate::metric::AUDIBLE_FLOOR;

/// What an object has to carry to be heard: the absolute floor the room
/// sets, and the relative one its neighbours do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Floor {
    /// The power below which nothing is heard at the playback level, in the
    /// unit [`crate::Object::energy`] carries: a mean square with full scale
    /// at one. Nought turns it off.
    pub absolute: f64,
    /// The fraction of the block's loudest object below which an object does
    /// not set a worst case.
    pub relative: f64,
}

impl Floor {
    /// The floor for a programme at this `dialnorm`, in decibels below full
    /// scale as the stream states it.
    pub fn for_dialnorm(dialnorm: f64) -> Self {
        let full_scale = FULL_SCALE_SPL + (DIALNORM_REFERENCE - dialnorm.clamp(-31.0, -1.0));
        let threshold = THRESHOLD_SPL - full_scale;
        Self {
            absolute: 10f64.powf(threshold / 10.0),
            relative: RELATIVE,
        }
    }

    /// The relative guard alone, which is what the floor was.
    pub const fn relative_only() -> Self {
        Self {
            absolute: 0.0,
            relative: RELATIVE,
        }
    }

    /// The absolute floor in decibels below full scale, for a report.
    pub fn threshold_dbfs(&self) -> f64 {
        if self.absolute > 0.0 {
            10.0 * self.absolute.log10()
        } else {
            f64::NEG_INFINITY
        }
    }

    /// Whether an object carrying `energy` in a block whose loudest object
    /// carries `loudest` is heard at all.
    pub fn audible(&self, energy: f64, loudest: f64) -> bool {
        energy > self.absolute && energy > loudest * self.relative
    }

    /// Whether an object carrying `energy` is above the absolute floor alone:
    /// audible in the room, whatever its neighbours.
    pub fn heard(&self, energy: f64) -> bool {
        energy > self.absolute
    }
}

impl Default for Floor {
    /// The floor for a programme at the reference: −102.6 dBFS.
    fn default() -> Self {
        Self::for_dialnorm(DIALNORM_REFERENCE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chain from the stream to the room, step by step: a programme at
    /// the reference is inaudible below −102.6 dBFS, and one the decoder
    /// turns down by eleven decibels below −91.6.
    #[test]
    fn the_threshold_follows_the_playback_level() {
        assert!((Floor::default().threshold_dbfs() + 102.6).abs() < 1e-9);
        assert!((Floor::for_dialnorm(-20.0).threshold_dbfs() + 91.6).abs() < 1e-9);
        // A dialnorm the syntax cannot state is clamped to what it can.
        assert_eq!(Floor::for_dialnorm(-40.0), Floor::default());
        assert_eq!(Floor::relative_only().threshold_dbfs(), f64::NEG_INFINITY);
    }

    /// Both guards apply: an object under the room's floor is not heard
    /// however quiet its neighbours, and one forty decibels under a loud
    /// neighbour is not heard however loud the room plays it.
    #[test]
    fn both_guards_apply() {
        let floor = Floor::default();
        let quiet = 10f64.powf(-11.0);
        assert!(!floor.audible(quiet, quiet));
        assert!(!floor.heard(quiet));
        let audible = 10f64.powf(-9.0);
        assert!(floor.audible(audible, audible));
        assert!(!floor.audible(audible, audible * 1e5));
        assert!(floor.heard(audible));
        // The relative guard alone hears the quiet one when it is the
        // loudest thing there is.
        assert!(Floor::relative_only().audible(quiet, quiet));
    }
}
