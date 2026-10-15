//! Joint object coding (ETSI TS 103 420): the parameters that rebuild a
//! programme's objects from the downmix E-AC-3 carries.
//!
//! For now the payload itself — its model, reader and writer, held byte for
//! byte to the reference streams. Estimating the parameters, the part that is
//! research, comes on top of it.

pub mod estimate;
pub mod huffman;
pub mod payload;
mod prototype;
pub mod qmf;
mod trees;

pub use payload::{Data, Joc, Object, SparsePoint, Unreadable, read};
