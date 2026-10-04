//! Where a fold puts its elements, block after block.
//!
//! A fold places fewer elements than there are objects, and places them
//! again every block. The placement is the clusterer's, warm-started from the
//! block before, and steered by the energies of the window around the block
//! rather than by the block alone — see `hz_cluster::smooth` — so that an
//! element moving because a sound *appears* is already there when it does.
//! That needs the energies of the blocks ahead before a block is placed,
//! which a writer gets by reading ahead of what it folds and recording each
//! block's energies as it is read ([`Placer::record`]). The rest — the mix,
//! the limiter, the cost — is [`crate::mix::Mixer`]'s.

use hz_cluster::smooth::{Smoother, Smoothing};
use hz_cluster::{Clusterer, Clustering, Object};
use hz_core::Result;

/// The clusterer, the window it is steered by, and the block it last placed.
pub struct Placer {
    clusterer: Clusterer,
    smoother: Smoother,
    /// The scene as the placement sees it — the block's geometry carrying the
    /// energies of the window around it. Refilled rather than rebuilt.
    placing: Vec<Object>,
    /// The last block's clustering: what the weights ramp *from*, and what
    /// the next one warm-starts from.
    previous: Option<Clustering>,
    /// How many blocks the fit bounded an element's coherent peak in.
    bounded: u64,
}

impl Placer {
    pub fn new(clusterer: Clusterer, smoothing: Smoothing) -> Self {
        Self {
            clusterer,
            smoother: Smoother::new(smoothing),
            placing: Vec::new(),
            previous: None,
            bounded: 0,
        }
    }

    pub fn smoothing(&self) -> Smoothing {
        self.smoother.smoothing()
    }

    /// Blocks of energy the placement wants to see beyond the one it is
    /// placing.
    pub fn look_ahead(&self) -> usize {
        self.smoother.smoothing().ahead
    }

    /// The energies of the next block read, in the order its objects are
    /// placed.
    pub fn record(&mut self, energies: &[f64]) {
        self.smoother.record(energies);
    }

    /// Place the next block, whose finished scene is `scene`: its geometry,
    /// and the energies of the window around it. The answer is not kept until
    /// [`Placer::keep`] — the mix ramps from the one before.
    pub fn place(&mut self, scene: &[Object]) -> Result<Clustering> {
        self.smoother.place(scene, &mut self.placing);
        let clustering = self
            .clusterer
            .cluster(&self.placing, self.previous.as_ref())?;
        if clustering.bounded > 0 {
            self.bounded += 1;
        }
        Ok(clustering)
    }

    /// The last block kept.
    pub fn previous(&self) -> Option<&Clustering> {
        self.previous.as_ref()
    }

    /// Keep `clustering` as the block placed last.
    pub fn keep(&mut self, clustering: Clustering) {
        self.previous = Some(clustering);
    }

    /// Blocks in which the fit bounded an element's coherent peak.
    pub fn bounded(&self) -> u64 {
        self.bounded
    }
}
