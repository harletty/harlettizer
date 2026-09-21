//! How a block is charged for descriptions it does not pay for alone.
//!
//! A filter is not described once per block: it is described once and kept
//! until it stops paying, and a matrix is described once an interval and kept
//! for the whole of it. So the honest price of a candidate is not its own
//! description — it is a *share* of it, and the share is how long the thing
//! lives.
//!
//! Nothing here is derived. Each constant is the best of a small set measured
//! over the fixtures, and the measurement is stated beside it so that the next
//! person to move one knows what moving it costs. They were scattered as bare
//! divisions in three different files, which is how a number ends up being two
//! numbers.
//!
//! # What this is not
//!
//! It is not the cost of writing something — that is [`BitCounter`] and the
//! writer functions, which cannot drift from each other. This is only the
//! amortisation on top: given what a description costs to write, what a block
//! choosing it should be charged.
//!
//! [`BitCounter`]: hz_core::bits::BitCounter

/// A block's share of the first filter's description.
///
/// A quarter, measured on how long one lives. Charging a block the whole of it
/// prices a filter as if it lived for one block, and the encoder then picks
/// filters too shallow to be worth describing at all. The quarter is the best
/// of 1, 2, 4 and 8 on four fixtures — worth about 3 % on three of them, and
/// 0.04 % worse than charging in full on the fourth. Eight does better on the
/// same three and half a per cent worse on the fourth, which is why it is not
/// eight.
const FIRST_FILTER_SHARE: usize = 4;

/// And of the second filter's.
///
/// A second filter is a fixed design that stays in force for as long as the
/// signal keeps its roll-off — an interval or more — so a block is charged a
/// far smaller part of it than of the first, or on quiet channels it never
/// enters at all: its whole description, history included, is a third of such
/// a block, and its gain a few bits.
const SECOND_FILTER_SHARE: usize = 4;

/// What a block is charged for the filters it would have to describe.
///
/// `first` and `second` are what writing each filter costs, in bits, or zero
/// for one the block does not restate — a filter already in force is free, and
/// that is the whole reason a deep filter can pay for itself over forty
/// samples when describing one afresh never could.
#[inline]
pub(crate) fn filters(first: usize, second: usize) -> usize {
    first / FIRST_FILTER_SHARE + second / SECOND_FILTER_SHARE
}

/// What a matrix is charged, given what one description of it costs.
///
/// `copies` is how many substreams have to describe it: a matrix is undone by
/// every presentation that reaches the channel it writes, and each of those
/// describes it for itself. All of them are paid for, so all of them are
/// costed — which is why a matrix on the stereo pair has to earn four
/// descriptions and one on the last substream only one.
///
/// No share here, and that is deliberate: a matrix is chosen in a restarting
/// unit and stands for the whole interval, but it is *also* costed against
/// that one unit's coding, so charging it in full is charging it against the
/// only evidence there is.
#[inline]
pub(crate) fn matrix(description: usize, copies: usize) -> usize {
    description * copies
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A filter in force costs a block nothing, which is the case the whole
    /// amortisation exists to make available.
    #[test]
    fn a_kept_filter_is_free() {
        assert_eq!(filters(0, 0), 0);
    }

    /// And the shares are shares: charging in full would price a filter as if
    /// it lived for one block.
    #[test]
    fn a_described_filter_is_charged_a_share_of_itself() {
        let description = 120;
        assert!(filters(description, 0) < description);
        assert_eq!(filters(description, 0), description / FIRST_FILTER_SHARE);
        assert_eq!(filters(0, description), description / SECOND_FILTER_SHARE);
    }

    /// A matrix every presentation has to undo is described by every one of
    /// them, and pays for each.
    #[test]
    fn a_matrix_pays_for_every_description_of_itself() {
        assert_eq!(matrix(40, 1), 40);
        assert_eq!(matrix(40, 4), 160);
    }
}
