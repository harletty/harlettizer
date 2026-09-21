//! One update to an object, as the encoder and the panner both read it.

/// How an object is to be rendered, beyond where it is and how loud.
///
/// The statements a delivery payload carries per element and a renderer
/// honours: whether to snap it to the nearest speaker, which zones of the
/// room it is confined to, whether its height counts, and how far it
/// follows the screen. Two objects that differ in any of these render
/// differently however close they sit, so a fold that shares an element
/// between them changes what one of them means — which is why the
/// clustering keeps them apart, and why an element's own metadata are one
/// object's and never a mixture. See `hz_cluster::class`.
///
/// Carried in the codes the payload writes rather than in the master's own
/// words, so that equality here is equality on the wire. The master's
/// vocabulary — `no back`, `screen only`, a screen factor of 0.375 — is
/// turned into these codes where the master is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Mode {
    /// Place it at the nearest speaker outright.
    pub snap: bool,
    /// Which zones of the room it is confined to: nought for none, then no
    /// back, no sides, centre and back, screen only, surround only.
    pub zones: u8,
    /// Whether its height is to be honoured.
    pub elevation: bool,
    /// How far it follows the screen rather than the room, and how far into
    /// it, in three bits and two. Absent when it follows the room.
    pub screen: Option<(u8, u8)>,
}

impl Default for Mode {
    /// What a master states when it states nothing: a free object, with its
    /// height honoured, following the room.
    fn default() -> Self {
        Self {
            snap: false,
            zones: 0,
            elevation: true,
            screen: None,
        }
    }
}

/// One update to an object: where it is, how loud, and how long to take
/// getting there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Keyframe {
    pub sample_pos: u64,
    /// ADM Cartesian, which is what a master set carries.
    pub position: [f64; 3],
    /// Linear, not decibels.
    pub gain: f64,
    /// Samples to reach the new gains. Zero means immediately.
    pub ramp_samples: u32,
    /// Object size, fed to the panner as spread.
    pub spread: f64,
    /// What the description says this object is worth, 0 to 1.
    ///
    /// Not used by the panner, which places everything it is given: it is
    /// carried because a *fold* needs it. A mix says which of its objects
    /// matter through this as well as through their level, and an encoder
    /// deciding which objects share an element has to weigh them the way the
    /// mix asked — see [`hz_cluster::scene`] in the sibling crate.
    ///
    /// One when the description says nothing, which is what an object nobody
    /// ranked should count as.
    pub importance: f64,
    /// How it is to be rendered, beyond where and how loud.
    ///
    /// Not used by the panner either, which pans everything as a free object:
    /// it is carried because a fold keeps objects that render differently
    /// apart, and writes each element's rendering as one object's.
    pub mode: Mode,
}

impl Default for Keyframe {
    fn default() -> Self {
        Self {
            sample_pos: 0,
            position: [0.0, 1.0, 0.0],
            gain: 1.0,
            ramp_samples: 0,
            spread: 0.0,
            importance: 1.0,
            mode: Mode::default(),
        }
    }
}
