# What the MLP encoder writes today

The state of `hz-mlp`, not its history: every constant and decision in force,
so a change is made against what is rather than against what a commit message
once said. [`mlp.md`](mlp.md) is the notebook — what was tried, what it
measured, what was put down — and where the reasons live.

## Shape of a stream

| | |
|---|---|
| Channel counts | 1, 2, 6, 8, and 9 to 16 as an object programme. Anything else is refused by name. |
| Sample rates | 44.1, 48, 88.2, 96, 176.4, 192 kHz |
| Sample width | 16 or 24 bits in; 24 bits throughout the codec, 16-bit input shifted up eight on the way in |
| Access unit | 40 samples at the base rate, doubling with each rate factor |
| Substreams | 1 below three channels, 2 up to eight, 4 for an object programme |
| Channel order | the format's, which is a WAV file's except at 7.1, where the sides go on the wire before the rears |
| Channel assignment | the identity — except an object programme carrying folds, whose last substream permutes the elements into the channels the cascade was built for and says so in its restart header, and whose earlier substreams say where each row of theirs comes out |

The substream split, and the restart sync word each carries:

| Channels | Substream 0 | 1 | 2 | 3 |
|---|---|---|---|---|
| 1–2 | all, A | | | |
| 6 | 0–1, A | 2–5, A | | |
| 8 | 0–1, A | 2–7, B | | |
| 9–16 | 0–1, A | 2–5, B | 6–7, B | 8–last, C |

Sync word A is refused above five matrix channels, which is why eight channels
put B on substream 1.

## Framing

| Constant | Value | What it is |
|---|---|---|
| `RESTART_INTERVAL` | 128 units | Between one restart header and the next. The format's own cap, which a reference stream sits at. |
| `RESTART_BLOCK` | 8 samples | The unpredicted first block of a restarting unit, which refills the decoder's filter state from its own data. Eight is the longest filter the format has, and what the format says a decoder's block size is after a restart header. |
| `INPUT_RESERVE` | 8 units | How much of the decoder's input buffer the stream gives itself, for a large unit to borrow against. |
| Declared peak | max(9.6 Mbit/s, worst case), capped at 18.0 | A decoder sizes its input FIFO from this and refuses units that arrive faster. 18.0 is the most the field can say, and sixteen channels uncompressed exceed it — so an immersive stream is one the format requires to compress. |
| `MAX_OUTPUT_SHIFT` | 7 | The largest dead-bit shift a block may declare; the field is four bits and signed |
| `END_OF_STREAM` | `0xd234` | The marker a shortened final unit carries, the count in the low thirteen bits of the word after it. |

A restarting unit is two blocks — the unpredicted eight, then the rest. Every
other unit is one.

`Encoder::push` takes one access unit and returns **nothing** for the first 127
of every 128; the 128th returns a whole interval at once — **the one three
before**. That is the API, not an implementation detail: everything decided per
interval — the matrices, each channel's second filter, `max_shift`,
`max_output_bits` — is decided on the interval it applies to, which has to
exist first; and the encoder works on four intervals at once, one's folds
worked out while the one before it is prepared, the one before that is decided
block by block and the one before that is written. Each of the three reads nothing the others change, so the stream is
byte for byte what one at a time wrote. Folds beside writing took a quarter of
an encode off the clock with folds (the programme slice 1.96 s to 1.48 s, five
minutes of an overlay 19.4 s to 14.3 s); writing beside deciding took the
five minutes to 11.8 s, with the autocorrelation's lanes; deciding the blocks
beside the preparing, on a pool of their own, took them to 10.1 s. `finish` takes a
last, possibly short, unit and hands back everything not yet handed back. A
caller appends what it is given either way, and reads the statistics once it
has finished. Latency: 512 units.

## What is written, and what is left unsaid

The presence flags are `0xff` and are never restated. Within them a block
states a field only when it differs from what [`crate::model`] says the decoder
holds: the block size, the output shifts, the matrices, and per channel both
filters, the codebook and the width. A channel with nothing new to say costs
one bit rather than eleven.

Never written: the residual offset field (measured — larger than leaving it at
zero), the quantisation step, matrix dither, bypassed low bits, and
noise-channel coefficients. The immersive syntax's shift on a whole matrix is
written where a step of the cascade needs a coefficient past two, and nowhere
else. Written once per restart: the matrices, and one bit of the
high-resolution output timing field, a run-length code serialised across
headers.

## Prediction

| Constant | Value | What it is |
|---|---|---|
| `MAX_ORDER` | 8 | Taps in the first filter, over past samples |
| `MAX_IIR_ORDER` | 4 | Taps in the second, over past residuals |
| `PRECISION` | 15 | Bits a quantised coefficient is held to |
| `MIN_SHIFT` / `MAX_SHIFT` | 8 / 15 | The accumulator shift both filters share, which is the first filter's |
| `FIT_CONTEXT` | 512 samples | How much past the fit sees beyond the block itself; 256 was measured on a tone, 512 over the fixture set |
| `MAX_HUFF_LSBS` | 24 | The widest residual; past it the filter made things worse and order zero is taken |
| `SECOND_FILTERS` | `[0.83]`, `[1.5, 0.6]`, `[1.8, 0.85]`, `[1.72, 0.77]` | The four fixed second-filter designs. Not fits: a pair of poles that lifts a steep low-pass roll-off. The first three are the best of a measured grid; the fourth is the pair a shipped stream carries. |
| `SECOND_CONTEXT` | 2048 samples | Coded past each channel's history keeps, which is the context the next interval's decisions start from |
| `TRIAL_BLOCKS` | 44 | Blocks of the interval *ahead* coded in each mode to judge them |
| `TRIAL_MARGIN` | 0 | How much a design has to win by |
| `FAST_DESIGN` | 3 | The one design `Effort::Fast` tries, and only at every other restart: the shipped stream's own pair, which is the one taken most often when all four are offered |
| `WARM_UP` | 64 samples | How far a second filter is run to find the history it would install |
| `MAX_STATE_BITS` | 15 | The widest value an installed history can carry |

Every order is tried and the residuals actually computed, not estimated from
the fit's error: a coding pays for the *largest* residual in the block, which
sets the field width every sample is written at.

The second filter is decided once an interval, by coding in every mode the
interval the encoder is **about to write**, prepared exactly as it will be
coded. Only its context looks backwards — the fit needs samples before the
first block it judges, and those are the previous interval's tail, which is
also the state every filter choice in the new interval starts from. The first
filter is decided per block, fitted to what the second leaves.

## Entropy coding

Three codebooks and codebook zero, chosen per block together with the width
under them. The narrowest width each codebook can hold is found by halving
rather than by walking up from zero, then `SWEEP` = 4 widths above it are
costed, because a wider field costs a raw bit a sample and buys shorter codes.

## Matrices

| Constant | Value | What it is |
|---|---|---|
| `FRACTION` | 14 | Fractional bits a coefficient is written in |
| `MAX_MATRICES` | 15 / 16 | The count field is four bits; syncs A and B write it as itself, C as one less |
| `MAX_FIT_SOURCES` | 3 | The most channels one matrix is fitted against; eight measured 0.1 % worse |
| `RANK_SAMPLES` | 2048 samples | Stride for *ranking* candidate sources; the weights that go on the wire are fitted on the whole interval |
| `MATRIX_SEGMENTS` | 4 | Slices of the interval a moving weight is fitted on |
| `TRIAL_UNITS` | 4 | Units of the interval, spread evenly, a candidate is coded on to judge it; the description is charged in the same proportion, four in a hundred and twenty-eight |
| `MATRIX_MARGIN` | 20 bits | What a matrix has to win each trial unit by, over coding the channel alone: half a bit a sample at the base rate. A candidate picked on four units of a hundred and twenty-eight is the one the sample favoured as much as the best one; measured, 80 over the four trials is 0.3 to 0.8 % off every programme fixture and nothing off the synthetic ones |
| `MAX_SHIFT` | 6 | The most a step of the cascade may scale a whole matrix by, as a power of two |
| `SEARCH_BUDGET` | 1024 | Attempts the arrangement's backtracking search may make before the interval is written without folds |

Decided once, on the whole interval being held, and kept for all of it —
suggested on the channels **as they are stored**: permuted, dead bits off, the
cascade taken off — the domain a decoder applies them in, since it puts the
dead bits back after them. Sources are picked greedily by **partial** correlation —
each judged on what is left after those already chosen are fitted and
subtracted, since judging them all against the destination picks three
channels carrying the same thing — and restricted to channels a decoder
reconstructing the destination already holds. That ranking is strided; the
weights are not. They are joint least squares, as three-by-three normal
equations with a pivot test relative to the energies, and one source keeps
its own exact integer estimator. One, two and three sources are each costed
by coding `TRIAL_UNITS` units of the interval both ways, and the cheapest that
beats coding the channel alone by `MATRIX_MARGIN` a trial unit is kept. Every substream's four-bit matrix
count is a budget: the coding matrices, a presentation's rows and the cascade
share it, and a destination no substream has room for is not tried. A
presentation is written at whichever of its own output shifts takes the
fewest rows, so the budget is not spent on scaling rows.

A matrix is declared by every substream that can reach its destination and
[`crate::rate::matrix`] charges it for each. Only an object programme's last
substream can state a step, and only for a destination no earlier substream
describes — and only on a source that has a coefficient, since the stream
carries steps for the sources its mask names and no other. **The step is measured on this interval, not the last one**: the
weight is fitted on each of `MATRIX_SEGMENTS` slices of it and a least-squares
line put through those four gives the coefficient at unit zero and the step. A
whole-interval fit gives the *average* weight, which for a ramp is half the
drift away from where it should start — +1.05 % against −2.64 %, in
[`mlp.md`](mlp.md).

## Amortisation, and dead low bits

A block is charged a share of a description it does not pay for alone, in
[`crate::rate`]: a quarter of each filter's, and the whole of a matrix's once
per substream that declares it. A filter already in force is free, which is
what lets a deep filter pay for itself over forty samples.

The dead low bits are taken off per channel per access unit and put back by the
decoder after the filters and the matrices. A silent block holds whatever shift
is in force rather than asking for the largest going, which keeps the coded
domain still across a quiet passage; the restart header declares the largest
its own interval takes.

## Speed

Encoder alone, release, best of three, on a Ryzen 9 9950X: **about 3 ms per
channel-second** at full effort. `hz-cli` and `xtask` build `hz-mlp` with its
`parallel` feature; the pool is the encoder's own and is sized to the machine.
`HZ_TIME=1` in the environment makes the encoder time its phases and print
them when the stream is finished.

**The fork is once an interval, never once an access unit.** Four things run
across the pool — the second-filter trials and the channel search, each a
thread per channel walking all 128 units; the matrices, a thread per
destination judging its candidates, then admitted in order under the
substream budgets; and preparing the units, a thread per unit, in three passes
with a serial one between the first two that hands each silent unit the shift
in force before it. While the channel search forked per access unit, 360 000
times over five minutes, a wider pool made the encode 21 % *slower*, and once
the fork moved out it made it 13 % faster.

What is left on one thread, on four minutes of a twelve-element programme
carrying folds: writing the units 1.7 s and building the presentations 1.2 s,
of 17 s at 1050 % of a thirty-two-thread machine. Writing will not parallelise
the obvious way — the residuals go out interleaved sample by sample across a
substream's channels, so there is no per-channel bit string to encode apart
and concatenate. And the per-channel phases are capped at a thread a channel,
because a channel's blocks are decided in order against the model's state;
splitting one across threads would write a different stream.

Per-fixture times are in [`mlp.md`](mlp.md).

## What is deliberately absent

Five rate levers were built, measured over the fixture set at both efforts and
put down: charging a candidate for having to say anything, the same charge
narrowed to a kept filter, offering a plain first filter alongside a second,
partitioning an access unit into more than one block, and replacing the
amortisation constant with each channel's own measured filter lifetime. Also
absent, and the only proper replacement for that constant: a dynamic program
over the interval's blocks, bounded above by what filter descriptions cost at
all — 0.20 % to 1.01 % of a stream. All the numbers, and the two conditions
that would make the dynamic program worth a second forward pass, are in
[`mlp.md`](mlp.md). The rule throughout: a lever winning on synthetic fixtures
and losing on `prog_*` is not taken.

## What checks it

`cargo test -p hz-mlp` decodes every presentation of every stream it writes
through the reference decoder in `tests/decoder/` and verifies the lossless
check each restart header carries. That decoder reads at most eight matrices
under restart sync words A and B, as FFmpeg does, not the fifteen the field
can say: a stream past eight is one no player outside Dolby's opens. `cargo
xtask mlp` is the oracle: FFmpeg (samples compared up to eight channels) and
`harletty` on real material, every presentation, non-zero on any mismatch.
Every stream it writes, and every one `cargo xtask overlay-check` writes, also
goes through `cargo xtask ffmpeg-check`: each substream's matrices counted
against what a decoder reads, and the whole stream decoded by FFmpeg at
`-v warning`, failing on any line it prints — its exit status is zero even on
a stream it refused throughout.
