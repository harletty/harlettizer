//! Arranging the elements so the leading channels carry the folds.
//!
//! A decoder stopping after a substream has decoded that substream's channels
//! and no others, so a presentation stated there can only read those. Declaring
//! a fold is therefore not enough: the leading channels have to *already carry*
//! what each fold needs. That is what a shipped stream's permuted internal
//! channel order is for, and this is how one is built.
//!
//! # The step, and why the round trip is exact whatever it does
//!
//! Every step here is one primitive matrix:
//!
//! ```text
//! stored[dest] += (Σ coefficient[i] · stored[i]) >> 14      for i ≠ dest
//! ```
//!
//! It changes `dest` and reads every channel *but* `dest`, so the sum is the
//! same quantity before and after it runs. An encoder that **subtracts that
//! same shifted sum** therefore undoes it exactly, whatever the coefficients
//! are and whatever the shift truncated.
//!
//! 🔴 Subtracting the sum is not the same as negating the coefficients. A
//! shift is a floor, so `(−x) >> 14` is `−⌈x/2¹⁴⌉` and not `−⌊x/2¹⁴⌋`: a
//! cascade built by negating loses a sample wherever the sum is not a multiple
//! of the shift, which is nearly everywhere. The two directions share one set
//! of coefficients and differ only in the sign of what they add.
//!
//! That is what makes the arrangement safe to attempt at all. Nothing below can
//! cost a bit of the programme; the worst a badly chosen coefficient can do is
//! make a presentation wrong, and the presentations are checkable.
//!
//! # Choosing the steps
//!
//! Write `U` for what the channels currently hold, as combinations of the
//! elements — the identity before anything runs. To make some channel carry a
//! fold row `f`, expand `f` in the rows of `U`:
//!
//! ```text
//! f = Σ g_i · U_i
//! ```
//!
//! and give the step to the channel `h` with the largest `|g_h|`, declaring
//! `−g_i / g_h`. Then the step's own destination coefficient works out to one —
//! which is what a lifting step is — and the channel ends up holding `f / g_h`,
//! a scalar multiple of the fold row. The presentation's own matrix puts the
//! scale back, since that one may say anything.
//!
//! Largest `|g_h|` is not a preference. The coefficients are divided by it, so
//! the channel stores the row **amplified by `1 / g_h`** — a host holding an
//! eighth of its row stores it eight times too loud, which is three bits a
//! sample on that channel and the headroom of the codec's domain gone. One
//! step says the whole row whatever the coefficients come to, through the
//! shift restart sync word C puts on a matrix ([`Primitive::wide`]); what a
//! small share costs is not steps but bits, and the caller's job is to put a
//! big contributor inside the presentation — see [`order_elements`].
//!
//! # And the host has to be inside the presentation
//!
//! A decoder stopping after the two-channel substream has channels 0 and 1 and
//! nothing else, so the stereo fold has to live in one of *those* — not in
//! whichever channel happens to contribute most. `within` says how many leading
//! channels each row may be hosted in, and the search is confined to them.
//!
//! Those two pull against each other, and what settles it is upstream of here:
//! **which element is stored in which channel**. Put the elements that
//! contribute most to a fold in the channels that fold may use, and the largest
//! contributor is inside the range by construction. That is what a shipped
//! stream's permuted internal channel order is, and choosing it is the caller's
//! job — [`arrange`] reports the failure rather than writing a row it cannot.

use crate::matrix::{FRACTION, MAX_SOURCES, Primitive, UNITY};

/// How many steps may be spent making a host for one row.
///
/// A helper recurses on the same row, so without a cap nothing ends it — the
/// first scene that needed two overflowed the stack. Two is enough for every
/// scene measured and a third has never helped.
const HELPED_AT_MOST: usize = 2;

/// How many rows the search may try to write before it gives up.
///
/// 🔴 The search backtracks, and a scene whose last block cannot be hosted
/// whatever the earlier blocks chose sends it through every combination of
/// their hosts and helpers before it says no: measured at fourteen million
/// back-outs in a minute on one interval of a programme, three and a half
/// minutes in — the whole encoder on one thread for as long as that interval
/// took, and it took minutes. A scene that can be arranged is arranged in a
/// few dozen attempts; a thousand is where one that cannot has proved it, and
/// costs a few milliseconds. Over the budget the interval is written without
/// folds, which is what it would have got after the minutes anyway.
const SEARCH_BUDGET: usize = 1024;

/// How far outside its own channels a presentation's row may still land.
///
/// Relative to the row's own size. Not zero, because the coefficients are
/// fourteen-bit — on a grid `2^k` coarser for a step scaled by `2^k` — and a
/// cascade accumulates a few of those: a fold is carried to the precision of
/// the field it is written with, and asking for exactness here would be asking
/// the quantiser for something it cannot give. What *is* exact is the
/// programme, which the round trip guarantees whatever these say. The same
/// figure as [`crate::hierarchy`]'s, for the same reason.
const SETTLED: f64 = 4e-3;

/// A cascade of steps, as the stream declares them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Arrangement {
    /// What a decoder applies to the stored channels to get the elements, in
    /// the order it applies them. The encoder undoes them the other way round
    /// with [`unapply`], which is the same coefficients subtracted.
    pub steps: Vec<Primitive>,
    /// Which channel each fold row was brought into, one entry per row.
    ///
    /// `None` where the row needed nothing: its fold already lay in the span of
    /// the channels its presentation carries, so that presentation's own matrix
    /// set can produce it with no channel of its own. Most rows of a real scene
    /// come back `None`, and a row that does need one needs it because its fold
    /// reaches elements the leading channels do not yet hold.
    pub hosts: Vec<Option<usize>>,
    /// What each host ended up holding, as a multiple of its fold row: the
    /// presentation's matrix divides it out.
    pub scales: Vec<f64>,
    /// What each stored channel ends up holding, over the elements.
    ///
    /// Not the rows that were asked for. A row already in the span of the
    /// channels its presentation carries is left alone, so those channels keep
    /// what they had — which is still enough, and is the point. This is what a
    /// presentation's matrix actually reads, so it is what any claim about a
    /// presentation has to be checked against.
    pub held: Vec<Vec<f64>>,
}

impl Arrangement {
    /// 🔴 How much more than what it was given this cascade stores.
    ///
    /// A host ends up holding its row divided by its own share, so a share of
    /// 0.36 stores nearly three times what came in — and the codec's domain is
    /// twenty-four bits, which a decoder enforces by refusing the stream
    /// outright ("positive saturation from huffman decode"). The dead bits a
    /// common output shift leaves are the only headroom there is, and a fold
    /// that already clips leaves none.
    ///
    /// Measured from what the channels really hold, not from the last step
    /// that wrote them. This used to report the host's share at the *last* of
    /// several clamped steps, which is near one by then whatever the first
    /// one divided by — and said 1.01x of a cascade storing a stereo row at
    /// eight times its size.
    ///
    /// Whether that is affordable is the caller's to judge, since only the
    /// caller knows how loud the programme is. What would settle it for good is
    /// the fold leaving headroom on purpose, which is a decision about the
    /// programme rather than about this cascade.
    pub fn amplification(&self) -> f64 {
        self.scales
            .iter()
            .fold(1.0f64, |most, scale| most.max(1.0 / scale.abs().max(1e-12)))
    }
}

/// Build the cascade that puts `folds` into the leading channels.
///
/// `folds[j]` is the `channels`-long row of gains that presentation channel `j`
/// takes from the elements — what [`hz_render::ObjectFold`] gives for one
/// output channel, in the order the elements are stored. Rows are taken in
/// order, so the widest presentation's rows must come after the narrower ones'
/// and each set must extend the last.
///
/// `within[j]` is how many leading channels row `j` may be hosted in: the width
/// of the presentation it belongs to, since a decoder stopping there has no
/// others to read.
///
/// `None` if a row cannot be hosted: every channel inside its presentation is
/// already carrying something, or the row is zero there, or the coefficients it
/// needs are outside what the field can say — which is the caller's cue to
/// store the elements in a different order.
pub fn arrange(folds: &[Vec<f64>], within: &[usize], channels: usize) -> Option<Arrangement> {
    if channels == 0 || channels > MAX_SOURCES || folds.len() > channels {
        return None;
    }
    if within.len() != folds.len() {
        return None;
    }
    // What each channel currently holds, as a combination of the elements.
    let mut held = vec![vec![0.0f64; channels]; channels];
    for (channel, row) in held.iter_mut().enumerate() {
        row[channel] = 1.0;
    }
    for fold in folds {
        if fold.len() != channels {
            return None;
        }
    }

    let mut out = Arrangement::default();
    let mut taken = vec![false; channels];
    let mut budget = SEARCH_BUDGET;
    if !settle(
        0,
        0,
        folds,
        within,
        channels,
        &mut held,
        &mut taken,
        &mut out,
        &mut budget,
    ) {
        if budget == 0 && std::env::var_os("HZ_FOLD").is_some() {
            eprintln!("fold: the search ran out of its {SEARCH_BUDGET} attempts");
        }
        return None;
    }
    // What each host really holds, as a multiple of its row — the least
    // squares scale between the two, since the coefficients went onto a grid.
    // The share recorded while the row was written is the share at that
    // moment, and a host given a share by a helper step holds the row divided
    // by that share, which is the number that matters.
    for (row, host) in out.hosts.iter().enumerate() {
        let Some(host) = host else { continue };
        let dot: f64 = folds[row]
            .iter()
            .zip(&held[*host])
            .map(|(f, h)| f * h)
            .sum();
        let norm: f64 = held[*host].iter().map(|h| h * h).sum();
        if norm > 0.0 {
            out.scales[row] = dot / norm;
        }
    }
    out.held = held;

    // The encoder builds them first to last; a decoder undoes them last to
    // first, so that is the order they are declared in.
    out.steps.reverse();
    Some(out)
}

/// Which element to store in which channel, so that each fold's biggest
/// contributors are inside the presentation that needs them.
///
/// [`order_elements_weighted`] with every element as loud as every other.
///
/// The other half of the arrangement, and the one that decides whether it can
/// be written at all. [`arrange`] must host a fold row in the channels its
/// presentation carries, and the coefficients it writes are divided by the
/// host's own — so if the elements that dominate the stereo fold are stored in
/// channels the stereo presentation cannot see, the row asks for coefficients
/// the field cannot say and the arrangement fails.
///
/// **A channel can only host a row that contains its element.** This is not a
/// preference to be traded off, it is arithmetic: a step never touches its own
/// destination's share, so channel `j` holds element `j` for as long as the
/// stream runs. Expand a fold row in what the channels hold and the coefficient
/// on channel `j` *is* the row's share of element `j` — zero if the row does not
/// reach it, and no cascade can make it otherwise. Store an element the stereo
/// fold does not use in channel 0 and the stereo row has no host, whatever else
/// is true.
///
/// So this is an assignment and it is solved as one. Each row takes the lowest
/// free channel inside its presentation — that much is forced — and what is
/// chosen is which *element* goes there. Rows offer edges to every element they
/// reach, best first, and augmenting paths settle it: a row that finds its
/// choices taken pushes the earlier claimant onto another of its own.
///
/// 🔴 Greedy does not do. Serving the rows in presentation order hosts five of
/// seven on a twelve-element scene, and the two it drops are the 7.1's: the
/// 5.1's surround rows take the elements the back rows needed, having six other
/// elements they could have taken instead. Serving the most constrained row
/// first drops the same two. Only a matching finds it.
///
/// Returns `channel_of[element]`, which is what a caller writes its samples
/// through.
pub fn order_elements(folds: &[Vec<f64>], within: &[usize], elements: usize) -> Vec<usize> {
    order_elements_weighted(folds, within, elements, &vec![0.0; elements])
}

/// [`order_elements`], knowing how loud each element is.
///
/// `loudness[element]` is `log2` of the element's amplitude, or whatever is
/// proportional to the bits a channel holding it raw would cost.
///
/// # Why loudness is in it
///
/// A channel hosting row `k` on element `h` stores `row / g_h`, which costs
/// `bits(row) − log2|g_h|`; the elements no row is hosted on are stored as
/// they are, at `bits(e)` each. Over the whole cascade the rows' own bits are
/// a constant, and so is the sum of every element's — so what an assignment
/// decides is `Σ_k log2|g_{k,h_k}| + loudness(h_k)`, to be made as large as
/// it can be. That is a maximum-weight matching and it is solved as one.
///
/// 🔴 It is not "the biggest share". Hosting a row on a quiet element that
/// carries all of it stores the row once and the loud element raw; hosting it
/// on the loud element at a third stores the row at three times its size and
/// the quiet element raw — and the second is the cheaper by several bits a
/// sample, because a third costs a bit and a half and a loud element costs
/// eight more than a quiet one. The weight says so, and a share-only matching
/// measured on a real scene put the two silent elements of twelve in the
/// stereo pair's channels, hosting its rows at a sixteenth.
pub fn order_elements_weighted(
    folds: &[Vec<f64>],
    within: &[usize],
    elements: usize,
    loudness: &[f64],
) -> Vec<usize> {
    place_elements(folds, within, elements, loudness, &vec![false; folds.len()])
}

/// [`order_elements_weighted`], where some rows are **slots**: channels no
/// fold row takes, which hold an element as it is — and any element will do.
///
/// A slot weighs nothing on any element. The cost of the whole cascade is the
/// rows' own bits, less each host's share, plus each slot's element stored as
/// it is — and the last term is every element's loudness less the hosts', so
/// it is already inside the hosts' weights. Deciding the slots' elements in the
/// same matching as the hosts is what makes the choice right: picked first,
/// element by element, a slot takes a middling host away from the one row
/// that could use it — measured as a back row hosted on a silent element at
/// a sixteenth, because the element it shared with the other back row was
/// filling a channel that any other would have filled as well.
///
/// 🔴 And a slot must not be *rewarded* for a loud element, which it was for
/// an afternoon: that counts the element's loudness twice, once as a host's
/// gain and once as the slot's, and sends the loud elements to the slots. On
/// the second half of a programme it hosted the 5.1's surrounds on two
/// elements of four and a half bits — storing the fold, at twelve — while the
/// two loud elements they were folded with sat raw in the last substream at
/// ten.
pub fn place_elements(
    folds: &[Vec<f64>],
    within: &[usize],
    elements: usize,
    loudness: &[f64],
    slot: &[bool],
) -> Vec<usize> {
    let mut used = vec![false; elements];

    // The channel each row will take: the highest free one inside it, which is
    // what [`arrange`] will pick and why.
    let mut channel_for = vec![usize::MAX; folds.len()];
    for (row, channel) in channel_for.iter_mut().enumerate() {
        let inside = within.get(row).copied().unwrap_or(elements).min(elements);
        if let Some(free) = (0..inside).rev().find(|channel| !used[*channel]) {
            *channel = free;
            used[free] = true;
        }
    }

    // What each row is worth on each element: its share there, and how loud
    // the element is — both as bits. An element a row does not reach is out of
    // the question, and said so by a weight no real one comes near.
    let hosted: Vec<usize> = (0..folds.len())
        .filter(|row| channel_for[*row] != usize::MAX)
        .collect();
    let weights: Vec<Vec<f64>> = hosted
        .iter()
        .map(|row| {
            (0..elements)
                .map(|element| {
                    let loud = loudness.get(element).copied().unwrap_or(0.0);
                    if slot.get(*row).copied().unwrap_or(false) {
                        return 0.0;
                    }
                    let share = folds[*row].get(element).copied().unwrap_or(0.0).abs();
                    if share > 1e-9 {
                        share.log2() + loud
                    } else {
                        FORBIDDEN
                    }
                })
                .collect()
        })
        .collect();
    let matched = assign(&weights, elements);

    let mut channel_of = vec![usize::MAX; elements];
    for (which, row) in hosted.iter().enumerate() {
        let element = matched[which];
        if element < elements && weights[which][element] > FORBIDDEN / 2.0 {
            channel_of[element] = channel_for[*row];
        }
    }

    // Whatever no row asked for keeps its own order behind them.
    let mut free: Vec<usize> = (0..elements).filter(|channel| !used[*channel]).collect();
    free.extend((elements..elements).collect::<Vec<_>>());
    let mut next = free.into_iter();
    for channel in channel_of.iter_mut() {
        if *channel == usize::MAX {
            *channel = next.next().unwrap_or(0);
        }
    }
    channel_of
}

/// The weight of a pairing that cannot be made.
const FORBIDDEN: f64 = -1.0e9;

/// The maximum-weight matching of rows to columns, `weights[row][column]`,
/// with at most as many rows as columns: which column each row takes.
///
/// The Hungarian method in its potential form, minimising `−weight`. Sixteen
/// rows at the very most and run once per restart, so what it costs is of no
/// interest; what matters is that it is the optimum, since a greedy pass with
/// augmenting paths finds *a* matching and not the best one, and the best one
/// is what the weights were built to say.
fn assign(weights: &[Vec<f64>], columns: usize) -> Vec<usize> {
    let rows = weights.len();
    if rows == 0 || rows > columns {
        return vec![usize::MAX; rows];
    }
    // One-based, as the classic statement is; `way` remembers the augmenting
    // path and the potentials `u`, `v` keep every reduced cost non-negative.
    let cost = |row: usize, column: usize| -weights[row - 1][column - 1];
    let mut u = vec![0.0f64; rows + 1];
    let mut v = vec![0.0f64; columns + 1];
    let mut taken_by = vec![0usize; columns + 1];
    let mut way = vec![0usize; columns + 1];
    for row in 1..=rows {
        taken_by[0] = row;
        let mut column = 0usize;
        let mut least = vec![f64::INFINITY; columns + 1];
        let mut used = vec![false; columns + 1];
        loop {
            used[column] = true;
            let here = taken_by[column];
            let mut delta = f64::INFINITY;
            let mut next = 0usize;
            for candidate in 1..=columns {
                if used[candidate] {
                    continue;
                }
                let reduced = cost(here, candidate) - u[here] - v[candidate];
                if reduced < least[candidate] {
                    least[candidate] = reduced;
                    way[candidate] = column;
                }
                if least[candidate] < delta {
                    delta = least[candidate];
                    next = candidate;
                }
            }
            for candidate in 0..=columns {
                if used[candidate] {
                    u[taken_by[candidate]] += delta;
                    v[candidate] -= delta;
                } else {
                    least[candidate] -= delta;
                }
            }
            column = next;
            if taken_by[column] == 0 {
                break;
            }
        }
        // Walk the path back, giving each column on it to the row before.
        loop {
            let previous = way[column];
            taken_by[column] = taken_by[previous];
            column = previous;
            if column == 0 {
                break;
            }
        }
    }
    let mut out = vec![usize::MAX; rows];
    for column in 1..=columns {
        if taken_by[column] != 0 {
            out[taken_by[column] - 1] = column - 1;
        }
    }
    out
}

/// A fold row, read through an ordering: `out[channel_of[e]] = fold[e]`.
pub fn through(fold: &[f64], channel_of: &[usize]) -> Vec<f64> {
    let mut out = vec![0.0; channel_of.len()];
    for (element, gain) in fold.iter().enumerate() {
        if let Some(channel) = channel_of.get(element)
            && *channel < out.len()
        {
            out[*channel] = *gain;
        }
    }
    out
}

/// Host one row and then the rest, backing out of a host that leaves no room.
///
/// 🔴 The host cannot be chosen row by row and kept. A channel's share of a row
/// is its coordinate in the basis the channels *currently* hold, and every row
/// hosted changes that basis — so a channel that holds exactly the element a
/// later row needs can still end up with a coordinate of zero on it, once the
/// rows before have taken that element's contribution through channels of their
/// own. Measured on a twelve-element scene: with the 5.1's surrounds hosted, the
/// 7.1's back rows have a coordinate of **exactly zero** on the two channels
/// left to them, though those channels hold the very elements they are made of.
///
/// So the choice is searched rather than decided. Each row tries its channels
/// best first and hands the rest of the rows the basis it made; a row that
/// cannot be hosted at all sends the one before it to its next choice. The
/// search is over at most sixteen rows and is run once for a whole stream, so
/// what it costs does not matter.
#[allow(clippy::too_many_arguments)]
fn settle(
    row: usize,
    // How many steps have already been spent making a host for *this* row.
    helped: usize,
    folds: &[Vec<f64>],
    within: &[usize],
    channels: usize,
    held: &mut [Vec<f64>],
    taken: &mut [bool],
    out: &mut Arrangement,
    budget: &mut usize,
) -> bool {
    // 🔴 A block that has just closed has to be checked, and a skipped row is
    // why. A row is left alone when its fold already lies in the span of the
    // channels its presentation carries — but a *later row of the same
    // presentation* is then hosted in one of those very channels and rewrites
    // it, and the span the skip was granted against is gone. Measured: a
    // cascade that every step of reported success, and whose 5.1 missed its own
    // rows by 2.85.
    //
    // Once a presentation's channels are all settled nothing can reach them
    // again — later rows are confined to their own blocks and helper steps only
    // write above — so checking here is checking once and for good.
    if row > 0 && (row >= folds.len() || within[row] > within[row - 1]) {
        let closed = within[row - 1].min(channels);
        for (earlier, fold) in folds.iter().enumerate().take(row) {
            if within[earlier] != within[row - 1] {
                continue;
            }
            let size = fold.iter().fold(1e-12f64, |m, value| m.max(value.abs()));
            let Some(g) = expand(held, fold) else {
                return false;
            };
            if let Some(leak) = g
                .iter()
                .skip(closed)
                .map(|coordinate| coordinate.abs() / size)
                .filter(|leak| *leak > SETTLED)
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            {
                if std::env::var_os("HZ_FOLD").is_some() {
                    eprintln!(
                        "fold: the block of {closed} closed with row {earlier} leaking {leak:.2e}"
                    );
                }
                return false;
            }
        }
    }
    let Some(fold) = folds.get(row) else {
        return true;
    };
    let inside = within[row].min(channels);
    // 🔴 And a row may only be hosted in the channels its **own** presentation
    // adds. Hosting anywhere inside its reach looks harmless and is not: a 7.1
    // row hosted in channel 4 rewrites a channel the 5.1's prefix contains, and
    // the 5.1's rows — which were left alone because they were already in that
    // prefix — are no longer in it. Measured before this rule went in: the
    // stereo carried its fold to 1.6e-5 and the 5.1 missed its own by 1.8.
    //
    // The bound comes from the `within` list itself: the widest presentation
    // narrower than this row's is where this row's presentation begins.
    let from = within
        .iter()
        .filter(|carried| **carried < within[row])
        .max()
        .copied()
        .unwrap_or(0)
        .min(channels);
    let Some(g) = expand(held, fold) else {
        return false;
    };

    // 🔴 A row does not need a channel of its own. What a presentation needs is
    // that its fold lie in the **span** of the channels it carries, because its
    // own matrix set can then produce it; hosting is only how a row that does
    // not yet lie there is brought in. So a row whose mass is already inside
    // costs nothing and takes nothing.
    //
    // This is not a rare case, it is most of them. Measured on a twelve-element
    // scene: once the 5.1's surrounds are hosted, both of the 7.1's back rows
    // are already inside — a 7.1 fold of a scene like that needs no channel the
    // 5.1 did not already have. Demanding a host per row fails on exactly those
    // two, and fails for a reason that is not there.
    if g.iter()
        .skip(inside)
        .all(|coordinate| coordinate.abs() <= 1e-9)
    {
        // 🔴 And the channels of its own block that carry it are reserved. A
        // row that needs no step is one the channels already hold — the
        // centre of a 5.1 sitting in the channel its element was put in — and
        // a channel left free is the next row's best host: the centre is gone
        // once the block closes, the check at the top says so, and the search
        // backs out through every host and helper the block had before it
        // gives up. Fourteen million back-outs on one interval, measured.
        // Reserved, the block closes the first time. Channels below the block
        // are closed already; the ones the row needs inside it are whichever
        // its expansion has a real part on.
        let size = fold.iter().fold(1e-12f64, |m, value| m.max(value.abs()));
        let reserved: Vec<usize> = (from..inside)
            .filter(|channel| !taken[*channel] && g[*channel].abs() > SETTLED * size)
            .collect();
        for channel in &reserved {
            taken[*channel] = true;
        }
        out.hosts.push(None);
        out.scales.push(1.0);
        let reached = settle(
            row + 1,
            0,
            folds,
            within,
            channels,
            held,
            taken,
            out,
            budget,
        );
        if !reached {
            out.hosts.pop();
            out.scales.pop();
            for channel in &reserved {
                taken[*channel] = false;
            }
        }
        return reached;
    }

    // Which channels could host it, best first. The low channels are the
    // scarce thing — a stereo row can only be hosted in two of them, while a
    // 7.1 row may go anywhere — so a row prefers the **highest** channel it may
    // use, which is to say each presentation's rows settle in the channels that
    // presentation adds. Among those, a share under [`WORTH_HOSTING`] of the
    // best going goes to the back of the queue rather than out of it: every
    // step's coefficients are divided by the host's share, so a small one costs
    // steps, but it is still better than failing.
    let free: Vec<usize> = (from..inside).filter(|channel| !taken[*channel]).collect();
    let mut candidates: Vec<usize> = free
        .iter()
        .copied()
        .filter(|c| g[*c].abs() > 1e-9)
        .collect();
    // Biggest share first, because the host ends up holding the row divided by
    // its own share and the stream has to carry that: a host with a share of
    // 0.16 stores six times what came in, and a codec whose domain is
    // twenty-four bits saturates. Measured as a decoder refusing the stream
    // outright — "positive saturation from huffman decode".
    candidates.sort_by(|a, b| {
        g[*b]
            .abs()
            .partial_cmp(&g[*a].abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for host in candidates {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        let kept = held[host].clone();
        let written = out.steps.len();
        if let Some(scale) = write_row(host, fold, channels, held, out) {
            taken[host] = true;
            out.hosts.push(Some(host));
            out.scales.push(scale);
            if settle(
                row + 1,
                0,
                folds,
                within,
                channels,
                held,
                taken,
                out,
                budget,
            ) {
                return true;
            }
            if std::env::var_os("HZ_FOLD").is_some() {
                eprintln!("fold: row {row} on channel {host} at {scale:.3} backed out");
            }
            taken[host] = false;
            out.hosts.pop();
            out.scales.pop();
        } else if std::env::var_os("HZ_FOLD").is_some() {
            eprintln!("fold: row {row} cannot be written on channel {host}");
        }
        held[host] = kept;
        out.steps.truncate(written);
    }

    // No host worked, but that is not the end of it. A channel's share of
    // a row can be zero because the rows before it already account for what it
    // holds — the 5.1's left, after the stereo fold is hosted, has a share of
    // exactly zero on all four channels the 5.1 adds. One step fixes it: mix
    // the would-be host into a channel *above* this presentation, and the
    // host's share becomes that channel's.
    //
    // Above, and not below or beside. Channels below belong to narrower
    // presentations, which have already been served and must not be disturbed;
    // channels beside are this presentation's own and are about to be. The ones
    // above belong to presentations that have not been built yet and read this
    // one's channels anyway, so what the step leaves there is theirs to work
    // with.
    //
    // 🔴 This is what makes storing the elements in their own order enough. The
    // alternative was to permute them so that every host has a share where it
    // needs one — which works, and costs a permuted channel assignment and an
    // object metadata order to match it, both of which are silent when wrong.
    if helped < HELPED_AT_MOST {
        let hosts = free;
        let helpers: Vec<usize> = (inside..channels)
            .filter(|channel| !taken[*channel])
            .collect();
        for host in hosts {
            for helper in helpers.iter().copied() {
                if g[helper].abs() <= 1e-9 {
                    continue;
                }
                if *budget == 0 {
                    return false;
                }
                *budget -= 1;
                let mut coefficients = vec![0.0f64; channels];
                coefficients[host] = 1.0;
                let Some(step) = step_of(helper, &coefficients) else {
                    continue;
                };
                let kept = held[helper].clone();
                let written = out.steps.len();
                let host_row = held[host].clone();
                for (mine, theirs) in held[helper].iter_mut().zip(&host_row) {
                    *mine -= theirs;
                }
                out.steps.push(step);
                if settle(
                    row,
                    helped + 1,
                    folds,
                    within,
                    channels,
                    held,
                    taken,
                    out,
                    budget,
                ) {
                    return true;
                }
                held[helper] = kept;
                out.steps.truncate(written);
            }
        }
    }
    false
}

/// Bring one fold into one channel, in one step.
///
/// The step declares `−g / g_host`, whatever that comes to: a row whose host
/// holds little of it asks for coefficients past two, and the immersive
/// syntax's shift on the whole matrix says them — see [`Primitive::wide`].
///
/// 🔴 This used to be several steps, each clamped to the field's own reach of
/// a little under two, repeated until the row was in. A real twelve-element
/// scene put five of them on one channel, which is five of a substream's
/// sixteen matrices spent on one row — and the coding matrices, which are
/// what make a dense channel cheap, were left one. The clamping also hid what
/// the row cost: the share reported was the share at the last step, near one,
/// while the channel held the row at eight times its size.
///
/// Returns the host's share, which is what the channel holds the row divided
/// by; [`arrange`] measures the real scale once the cascade is complete.
fn write_row(
    host: usize,
    fold: &[f64],
    channels: usize,
    held: &mut [Vec<f64>],
    out: &mut Arrangement,
) -> Option<f64> {
    let g = expand(held, fold)?;
    let here = g[host];
    if here.abs() <= 1e-9 {
        return None;
    }

    // The declared coefficients are the negatives of the expansion: the
    // encoder subtracts what the decoder adds, so a channel that has to
    // *gain* a contribution is declared with it and loses it here.
    let mut coefficients = vec![0.0f64; channels];
    for (channel, coefficient) in coefficients.iter_mut().enumerate() {
        if channel == host {
            continue;
        }
        *coefficient = -g[channel] / here;
    }
    let step = step_of(host, &coefficients)?;

    // What the host now holds, as the arithmetic will actually compute it:
    // the *quantised* coefficients, so the presentation matrix is built
    // against what the stream says rather than what was asked for.
    let mut now = held[host].clone();
    for (channel, holding) in held.iter().enumerate().take(channels) {
        if channel == host || step.coefficients[channel] == 0 {
            continue;
        }
        let quantised = f64::from(step.coefficients[channel]) / f64::from(UNITY);
        for (slot, take) in now.iter_mut().zip(holding) {
            *slot -= quantised * take;
        }
    }
    held[host] = now;
    out.steps.push(step);
    Some(here)
}

/// Solve `target = Σ g·rows` for `g`, by Gaussian elimination on the transpose.
pub(crate) fn expand(rows: &[Vec<f64>], target: &[f64]) -> Option<Vec<f64>> {
    let n = rows.len();
    let mut matrix = vec![0.0f64; n * n];
    for (row, held) in rows.iter().enumerate() {
        for (column, value) in held.iter().enumerate() {
            // Transposed: column `row` of the system is that row of `held`.
            matrix[column * n + row] = *value;
        }
    }
    let mut rhs = target.to_vec();

    for step in 0..n {
        let pivot = (step..n)
            .max_by(|a, b| {
                matrix[a * n + step]
                    .abs()
                    .partial_cmp(&matrix[b * n + step].abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(step);
        if matrix[pivot * n + step].abs() < 1e-12 {
            return None;
        }
        if pivot != step {
            for column in 0..n {
                matrix.swap(step * n + column, pivot * n + column);
            }
            rhs.swap(step, pivot);
        }
        for row in step + 1..n {
            let factor = matrix[row * n + step] / matrix[step * n + step];
            if factor == 0.0 {
                continue;
            }
            for column in step..n {
                matrix[row * n + column] -= factor * matrix[step * n + column];
            }
            rhs[row] -= factor * rhs[step];
        }
    }
    for step in (0..n).rev() {
        let mut value = rhs[step];
        for column in step + 1..n {
            value -= matrix[step * n + column] * rhs[column];
        }
        rhs[step] = value / matrix[step * n + step];
    }
    Some(rhs)
}

/// One lifting step: the destination at one, the rest as given, at whatever
/// shift the immersive syntax needs to say them.
fn step_of(dest: usize, coefficients: &[f64]) -> Option<Primitive> {
    let mut scaled = [0i32; MAX_SOURCES];
    if coefficients.len() > MAX_SOURCES {
        return None;
    }
    for (slot, coefficient) in scaled.iter_mut().zip(coefficients) {
        let value = (coefficient * f64::from(UNITY)).round();
        // Past what the widest shift reaches by a margin; `wide` says no to
        // anything the field cannot hold, and this only keeps the cast honest.
        if !value.is_finite() || value.abs() >= f64::from(1i32 << 28) {
            return None;
        }
        *slot = value as i32;
    }
    Primitive::wide(dest, &scaled[..coefficients.len()], FRACTION)
}

/// Run one step the way a decoder does: add the shifted sum to the
/// destination.
pub fn apply<P: AsRef<[i32]> + AsMut<[i32]>>(step: &Primitive, samples: &mut [P]) {
    apply_within(step, samples, usize::MAX);
}

/// [`apply`] over the first `frames` of each channel.
///
/// An encoder holds one access unit inside planes sized for the longest one, so
/// the whole plane is not the signal.
pub fn apply_within<P: AsRef<[i32]> + AsMut<[i32]>>(
    step: &Primitive,
    samples: &mut [P],
    frames: usize,
) {
    step_over(step, samples, 1, frames);
}

/// And the way an encoder does: subtract the same shifted sum.
///
/// Exactly the inverse of [`apply`], because a step reads every channel but
/// its own — so the sum is the same quantity either side of it, and one
/// truncation is added and the same one taken away.
pub fn unapply<P: AsRef<[i32]> + AsMut<[i32]>>(step: &Primitive, samples: &mut [P]) {
    unapply_within(step, samples, usize::MAX);
}

/// [`unapply`] over the first `frames` of each channel.
pub fn unapply_within<P: AsRef<[i32]> + AsMut<[i32]>>(
    step: &Primitive,
    samples: &mut [P],
    frames: usize,
) {
    step_over(step, samples, -1, frames);
}

/// Over planes that may be owned or borrowed — a unit's channels inside a
/// larger buffer as readily as vectors of their own.
fn step_over<P: AsRef<[i32]> + AsMut<[i32]>>(
    step: &Primitive,
    samples: &mut [P],
    sign: i32,
    frames: usize,
) {
    let channels = samples.len();
    if step.dest >= channels || samples[step.dest].as_ref().is_empty() {
        return;
    }
    let frames = frames.min(samples[step.dest].as_ref().len());
    for frame in 0..frames {
        let mut sum = 0i64;
        for (channel, plane) in samples.iter().enumerate() {
            if channel == step.dest {
                continue;
            }
            let coefficient = i64::from(step.coefficients[channel]);
            let plane = plane.as_ref();
            if coefficient != 0 && frame < plane.len() {
                sum += coefficient * i64::from(plane[frame]);
            }
        }
        let shifted = (sum >> FRACTION) as i32;
        samples[step.dest].as_mut()[frame] += sign * shifted;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cascade and its inverse return the samples exactly, whatever the
    /// coefficients and whatever the shift threw away.
    ///
    /// This is the property the whole arrangement rests on. It is asserted on
    /// awkward coefficients on purpose: if exactness depended on the numbers
    /// being nice, it would not be exactness.
    #[test]
    fn a_cascade_and_its_inverse_return_the_samples_exactly() {
        const CHANNELS: usize = 6;
        const FRAMES: usize = 97;

        // Fold rows with nothing round about them.
        let folds = vec![
            vec![0.31, -0.87, 0.12, 0.44, -0.05, 0.63],
            vec![-0.72, 0.19, 0.55, -0.28, 0.91, 0.07],
        ];
        let arrangement = arrange(&folds, &[2, 2], CHANNELS).expect("an arrangement");
        assert_eq!(arrangement.steps.len(), 2);

        let source: Vec<Vec<i32>> = (0..CHANNELS)
            .map(|channel| {
                (0..FRAMES)
                    .map(|frame| {
                        let n = (frame * 7919 + channel * 104_729) as i32;
                        (n % 16_777_216) - 8_388_608
                    })
                    .collect()
            })
            .collect();

        // The encoder undoes the declared steps, last to first.
        let mut stored = source.clone();
        for step in arrangement.steps.iter().rev() {
            unapply(step, &mut stored);
        }
        assert_ne!(stored, source, "the cascade did nothing at all");

        // And a decoder applies them in order and gets the elements back.
        for step in &arrangement.steps {
            apply(step, &mut stored);
        }
        assert_eq!(stored, source, "the round trip lost a sample");
    }

    /// And the leading channels carry the folds, which is what it is for.
    ///
    /// Not exactly: the coefficients are rounded to the field's grid and the
    /// shift truncates, so the channel holds the fold row to within that. What
    /// is asserted is that it holds *that row* rather than one of the others —
    /// the correlation with what was asked for is near one, and with an
    /// unrelated row it is not.
    #[test]
    fn the_leading_channels_carry_the_folds() {
        const CHANNELS: usize = 8;
        const FRAMES: usize = 512;
        let folds = vec![
            vec![0.5, 0.5, 0.35, 0.0, 0.25, 0.0, 0.2, 0.1],
            vec![0.0, 0.5, 0.35, 0.0, 0.0, 0.25, 0.1, 0.2],
        ];
        let arrangement = arrange(&folds, &[2, 2], CHANNELS).expect("an arrangement");

        // Independent-ish channels, so a host that carried the wrong row would
        // not correlate with the right one by accident.
        let source: Vec<Vec<i32>> = (0..CHANNELS)
            .map(|channel| {
                (0..FRAMES)
                    .map(|frame| {
                        let phase = (frame as f64) * (0.017 + 0.011 * channel as f64);
                        (phase.sin() * 2_000_000.0) as i32
                    })
                    .collect()
            })
            .collect();

        let mut stored = source.clone();
        for step in arrangement.steps.iter().rev() {
            unapply(step, &mut stored);
        }

        for (presentation, fold) in folds.iter().enumerate() {
            let host = arrangement.hosts[presentation].expect("a channel for this row");
            let scale = arrangement.scales[presentation];
            // What the fold asks for, sample by sample.
            let wanted: Vec<f64> = (0..FRAMES)
                .map(|frame| {
                    (0..CHANNELS)
                        .map(|channel| fold[channel] * f64::from(source[channel][frame]))
                        .sum::<f64>()
                })
                .collect();
            // And what the host holds, scaled back the way the presentation's
            // own matrix would scale it.
            let got: Vec<f64> = (0..FRAMES)
                .map(|frame| f64::from(stored[host][frame]) * scale)
                .collect();

            let error: f64 = wanted
                .iter()
                .zip(&got)
                .map(|(want, got)| (want - got) * (want - got))
                .sum::<f64>()
                .sqrt();
            let size: f64 = wanted.iter().map(|w| w * w).sum::<f64>().sqrt();
            assert!(
                error / size.max(1e-9) < 1e-3,
                "presentation channel {presentation} landed {:.5} away from its fold",
                error / size.max(1e-9)
            );
        }
    }

    /// Ordering the elements is what makes the arrangement writable.
    ///
    /// The same fold, the same two channels to host it in: stored as they come
    /// it cannot be written, and stored so the biggest contributors lead, it
    /// can. That is the whole of what a permuted channel order buys.
    #[test]
    fn ordering_the_elements_is_what_it_saves() {
        // Elements 2 and 3 dominate both folds; 0 and 1 barely contribute, and
        // 0 and 1 are the only channels the stereo presentation may host in.
        let folds = vec![vec![0.02, 0.01, 0.90, 0.10], vec![0.01, 0.02, 0.10, 0.90]];
        let within = [2usize, 2];

        // As they come it can still be written: the shift on a matrix says a
        // coefficient of forty-five in one step. What it cannot do is make
        // that cheap.
        let plain = arrange(&folds, &within, 4).expect("an arrangement");

        // What ordering saves is what the cascade costs to carry. The host ends
        // up holding its row divided by its own share, and a share of 0.02 is a
        // channel storing fifty times what came in — five and a half bits a
        // sample, and the codec's domain cannot hold it. Ordered, the loud
        // elements lead and the shares are near one.
        let channel_of = order_elements(&folds, &within, 4);
        assert!(channel_of[2] < 2, "the loudest of the first row is inside");
        assert!(channel_of[3] < 2, "and the loudest of the second is too");
        let reordered: Vec<Vec<f64>> = folds.iter().map(|f| through(f, &channel_of)).collect();
        let arrangement = arrange(&reordered, &within, 4).expect("an arrangement");
        assert_eq!(arrangement.hosts.len(), 2, "a channel each");
        assert_ne!(arrangement.hosts[0], arrangement.hosts[1]);
        // 🔴 This used to assert the two were within 0.05x of each other, and
        // they were — because the amplification was read off the *last* of the
        // clamped steps a row took, where the host's share is near one whatever
        // the first step divided by. Measured from what the channels hold, the
        // difference is the whole point of ordering.
        assert!(
            plain.amplification() > 10.0,
            "as they come, the host holds a fiftieth of its row: {:.2}x",
            plain.amplification()
        );
        assert!(
            arrangement.amplification() < 1.5,
            "ordered, the loudest element hosts its row: {:.2}x",
            arrangement.amplification()
        );

        // And it is still exact, which is the only thing that is never traded.
        let source: Vec<Vec<i32>> = (0..4i32)
            .map(|channel| {
                (0..64)
                    .map(|frame: i32| (frame * 31 + channel * 17) * 4_099 - 900_000)
                    .collect()
            })
            .collect();
        let mut stored = source.clone();
        for step in arrangement.steps.iter().rev() {
            unapply(step, &mut stored);
        }
        for step in &arrangement.steps {
            apply(step, &mut stored);
        }
        assert_eq!(stored, source);
    }

    /// The assignment is the best one, not the first one found.
    ///
    /// Two rows that both want the first element: greedy gives it to the
    /// first row and leaves the second with next to nothing, and the total
    /// says which is right.
    #[test]
    fn the_assignment_takes_the_best_total() {
        let weights = vec![
            vec![10.0, 1.0, FORBIDDEN, 3.0],
            vec![10.0, 9.0, FORBIDDEN, FORBIDDEN],
        ];
        assert_eq!(assign(&weights, 4), vec![0, 1]);
        let weights = vec![
            vec![10.0, 9.0, FORBIDDEN, FORBIDDEN],
            vec![10.0, 1.0, FORBIDDEN, 3.0],
        ];
        assert_eq!(assign(&weights, 4), vec![1, 0]);

        // And through the elements: two back rows sharing their biggest
        // element, one of them also reaching a silent one. Loud and shared
        // to one, loud and unshared to the other — never the silent one.
        let folds = vec![
            vec![0.0, 0.0, 0.0, 0.0, 0.84, 0.0, 0.0, 0.3, 0.0, 0.5, 0.0, 0.0],
            vec![0.0, 0.0, 0.0, 0.0, 0.84, 0.0, 0.0, 0.3, 0.5, 0.0, 0.0, 0.0],
        ];
        let mut loudness = vec![11.0; 12];
        loudness[8] = 0.0;
        loudness[9] = 0.0;
        let channel_of = order_elements_weighted(&folds, &[2, 2], 12, &loudness);
        assert!(channel_of[4] < 2, "the shared loud element hosts a row");
        assert!(channel_of[7] < 2, "and the other loud one hosts the other");
        assert!(
            channel_of[8] >= 2 && channel_of[9] >= 2,
            "the silent ones host nothing"
        );
    }

    /// A row nothing can host is refused rather than written wrong.
    #[test]
    fn a_row_with_nowhere_to_go_is_refused() {
        // 🔴 Reaching outside the presentation that wants it is not a refusal,
        // and used to be. This row reaches only elements the stereo cannot
        // read, so no channel it may use holds any share of it — and it is
        // still arranged, because one step above the presentation gives a
        // channel that share. What that step buys is the whole reason the
        // elements can be stored in their own order: the alternative was to
        // permute them, and to permute the channel assignment and the object
        // metadata to match, both of which are silent when wrong.
        let nothing_here = vec![vec![0.0, 0.0, 0.4, 0.9]];
        assert!(arrange(&nothing_here, &[2], 4).is_some());

        // What is refused is what cannot be counted: more rows than there are
        // channels to carry them.
        let too_many = vec![
            vec![1.0, 0.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0, 0.0],
            vec![0.0, 0.0, 1.0, 0.0],
        ];
        assert!(arrange(&too_many, &[2, 2, 2], 2).is_none());

        // And a row not written over the channels at all.
        assert!(arrange(&[vec![1.0, 0.0]], &[2], 4).is_none());
    }

    /// Each presentation gets a channel of its own, and the hosts are the
    /// largest contributors — which is what keeps the coefficients writable.
    #[test]
    fn every_presentation_takes_a_channel_of_its_own() {
        // Three nested presentations over four elements, each reaching one
        // element the one before it could not: two channels, then three, then
        // all four. Each row has to be brought into a channel of its own,
        // because each needs something the channels before it do not hold.
        let folds = vec![
            vec![0.9, 0.3, 0.2, 0.1],
            vec![0.2, 0.9, 0.3, 0.1],
            vec![0.1, 0.2, 0.9, 0.3],
        ];
        let arrangement = arrange(&folds, &[2, 3, 4], 4).expect("an arrangement");
        let hosts: Vec<usize> = arrangement.hosts.iter().flatten().copied().collect();

        // Two, not three. The last presentation carries every channel there
        // is, so whatever the stored channels hold it can read all of it and
        // its own matrix produces the row — a presentation that reaches
        // everything never needs a channel brought in for it. Only the two
        // that stop short do.
        assert_eq!(hosts.len(), 2, "a row was hosted that needed nothing");
        assert_eq!(arrangement.hosts[2], None, "the widest needed nothing");
        let mut distinct = hosts.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), 2, "two presentations shared a channel");
        for (row, host) in hosts.iter().enumerate() {
            assert!(*host < row + 2, "row {row} was hosted outside its own");
        }

        // And every coefficient is inside the field it is written in.
        for step in &arrangement.steps {
            for source in 0..4 {
                assert!(step.stored(source).abs() <= 1 << (step.frac_bits + 1));
            }
        }
    }
}
