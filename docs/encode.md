# Encoding a programme

```bash
harlettizer encode programme.atmos --out programme.thd
```

`--fast` searches less hard — one second-filter design, decided at every other
restart — for a quarter of a per cent of the stream and half the time; see
[mlp.md](mlp.md#the-second-prediction-filter-which-pays-after-all).

Takes a master set or an ADM BW64 file and writes the immersive presentation:
the elements coded losslessly, and the master's own object metadata carried in
the access units beside them.

Everything this needs already existed and nothing joined it up.
[`hz-io`](../crates/hz-io) reads a master set and projects its delta stream of
events into absolute per-update state, [`hz-meta`](../crates/hz-meta) writes
object audio metadata to the bit, [`hz-mlp`](../crates/hz-mlp) writes the
immersive bitstream. `encode` is the join: it walks the projected updates
alongside the audio and hands the encoder, for each access unit that needs one,
the payload saying where everything is.

## What it does with a real programme

A slice of a programme, decoded from its reference stream to a master set by
`truehdd --presentation 3` and encoded back:

```
  programme    11 objects and an LFE, 12 elements
  encoded      36106 access units, 1444240 samples
  metadata     282 payloads, one every 128 units
```

Decoded again, the elements come back **bit for bit** — 1 444 240 frames across
all twelve channels, largest difference zero — and the object positions come
back where the master put them, at the sample the master named.

## Reading a master's waveforms where they are

```bash
harlettizer encode vf.atmos --out vf.thd --overlay 5 --mono-prefix work/wave
```

A master set carries its waveforms interleaved in one file, and a delivered
one arrives that way. A master that is still being made — decoded from a
stream, re-voiced, re-mixed — is naturally one mono file per waveform, and
interleaving them only so that `encode` can read them back apart copies the
whole programme once more: some twenty gigabytes for a film of seventeen
waveforms. `--mono-prefix` reads them where they are instead, as
`<prefix>_0.wav`, `<prefix>_1.wav`… in the master's own order — its bed, then
its objects — which is the order an interleaved file holds them in and the name
`harletty decode --mono-prefix` writes them under.

The header still says which waveform is which and how many there are; only the
samples come from elsewhere. So every file has to be there, mono integer PCM,
all of one rate, one width and one length, and one file past the count the
header declares is refused as well: a master whose header and audio disagree is
one put together wrong, which is better said than read.

It changes where the samples come from and nothing else. Twenty-two waveforms,
encoded once from their CAF and once from the mono files that CAF was built
from, gave the same stream byte for byte.

## Folding a scene into the elements a stream carries

```bash
harlettizer encode crowd.atmos --out crowd.thd --cluster 12
```

Without `--cluster` a stream is one element per object, which only works
because every master to hand is already the output of somebody else's
clustering — eleven to fifteen objects, twelve to sixteen elements. A mix has
as many objects as it needs, and a delivery bitstream carries twelve, fourteen
or sixteen.

With it, [`hz-cluster`](../crates/hz-cluster) decides which elements carry
which objects, [`hz_cluster::mix`] makes the elements' audio, and
`Clustering::to_metadata` says where they are. The low frequency channel is not
clustered: it is a bed, it takes the first element untouched, and the payload
says so.

Forty objects, four seconds, over every presentation the stream will be played
through:

```
  programme    40 objects and an LFE, 12 elements
  fold         0.0527 mean over the presentations, 0.332 at its worst
  fold         0.0359 mean at fourteen elements
  fold         0.0256 mean at sixteen
```

Two things it reports because they are worth knowing rather than swallowing.
The **fold's cost** is the metric of [clustering.md](clustering.md), run on the
scene against the elements it was folded into, block by block — an encoder that
folds a scene and does not say what it cost is asking to be trusted. And
**clipping**: an element is a *sum*, and a sum of objects that happen to agree
is louder than any of them, so a mix can push past the codec's twenty-four
bits. Those samples are clamped rather than wrapped — a wrap comes out at the
other end of the range, which a decoder reports as a saturated recorrelator and
a listener hears as a crack — and counted.

The chain runs end to end: master, `--cluster 12`, TrueHD, `harletty decode
--presentation 3`, a master set again, with no decoder complaints.

[`hz_cluster::mix`]: ../crates/hz-cluster/src/mix.rs

## `--overlay`: keeping a scene and adding to it

A programme is re-voiced. The original's elements are to come through
untouched and a handful of new sources — a dubbed dialogue, one object per
channel of the track it came from — have to join them.

`--cluster` does this and costs more than it should. Asked for the number of
elements the original had, with more objects than that, it re-places **every**
element: the untouched part of the programme is re-mixed and re-quantised
because something was added to the other part, and a source at a place no
element occupies pulls a real element towards it. The original scene is
degraded to make room for the dub.

```bash
harlettizer encode vf.atmos --out vf.thd --overlay 3 --fold-depth 20
```

`--overlay K` says the master's **last K objects are sources** and everything
before them is an element to be kept. The elements stay where the master put
them and the sources are panned onto them, treating the element positions as a
time-varying loudspeaker set — clustering with frozen centres. See
`hz_cluster::overlay`.

### The input contract

One master set or ADM BW64 file, as usual, with its channels in this order:

| channels | what they are |
|---|---|
| `0 .. E-1` | the original's elements, in its order, with its metadata verbatim — the low frequency channel first, as the payload's own flag says |
| `E .. E+K-1` | the K sources, already at the level they are to arrive at |

`E` has to be between 3 and 16. The master itself may be wider than 16, which
a plain encode refuses: the sources ride after the elements and are not
elements. A bed other than the LFE is carried, so an original decoded with
`--bed-conform` works. `--overlay` and `--cluster` are mutually exclusive.

### What it guarantees

- **The elements' metadata is the master's**, element for element and unit for
  unit — position, gain, size, snap, zones, at the sample the master moved
  them: the payload rides the unit that holds the move and says how far into
  it the move falls, which is what a reference stream does and what a decoder
  dates the metadata from. `the_elements_state_exactly_what_the_master_said_of_them`
  pins the fields, `an_element_is_restated_at_the_unit_the_master_moved_it_in`
  the unit and `a_payload_takes_effect_at_the_sample_the_master_moved_at` the
  sample. `--cluster` keeps neither: it states the positions its own fold
  settled on, carries none of the rest, and can only speak once a block
  because that is how often it decides anything.
- **An element no source reaches is copied**, not mixed and *not rounded* to
  `--fold-depth`. On a dub that is most of the programme, and it is the
  difference between a re-voiced stream and a re-encoded one. The low
  frequency channel is always copied, and so is an element a source took a
  spare slot in under `--overlay-spare`.
  `an_element_no_source_reaches_is_copied_bit_for_bit` pins it.
- **A source arrives at the level it asked for.** An element's contribution is
  divided by the gain the *syntax* states for that element — whole decibels,
  which is what a decoder will apply — so a dub does not follow a gain the
  original mix wrote for something else.
- **The presentations fold the same metadata by the same functions** as they
  would have for the original, since the metadata is the same.

The summary says how much came through untouched:

```
  overlay      16 elements kept, 2 sources panned onto them
  untouched    77.8 % of the element-blocks copied through, neither mixed nor rounded (934 of 1200)
  drift        8.20° mean between where a source asked to be and where its carriers put it, 19.96° at its worst, over 2.00 audible sources a block
  beds         7 of 16 elements are bed channels, all 7 of them worked out from the master — static, and exactly at a speaker's place on the wire's own grid — since it declares none
```

`--overlay-report <path>` writes the account behind those figures, block by
block: which master channel each element's own audio is, which elements were
copied and which mixed, and the weights and gains each block mixed with. It is
what `cargo xtask overlay-check` replays against the decoded stream — the
checker does not derive the weights itself, since a checker that re-derived
the fit would be checking the fit against itself — and it is the only place
the element-to-channel mapping is written down, which a stream needs because
it carries the low frequency channel first whatever the master did.

One line each, tails included: a caller parses these, so what is printed here
is what the encoder prints.

### The cadence is the master's too

A fold can only speak once a block, because that is how often it decides
anything: its elements are placed for the block and reach their places across
it. An overlay places nothing — the elements are the master's — so there is
nothing tying its metadata to the block at all, and it writes one payload per
access unit, exactly where the master moved something.

That was not always so, and the difference was measurable: read once a block, a
keyframe at sample 48 000 landed at 47 360, thirteen milliseconds early on the
nearest block boundary, and any keyframe sharing a block with another was
dropped. The **mix** is still decided per block — that is what the weights ramp
over — and it does not need to agree with the metadata, because an overlay's
metadata is a copy and not a result.

### What it costs

A source whose direction no element covers at some block is spread over the
nearest ones, and those may be moving. A static source carried by moving
elements **breathes**: it is rendered somewhere slightly different every block
although it never asked to move. That is the one real cost, and `drift` is
what measures it.

`--overlay-beds` is what keeps it small. A bed element is still, so where the
beds can reach a source they are given it even when a moving object sits
closer this block. The rule is an absolute reach and not a comparison between
the two fits, because a comparison *cannot* decide this case: an object
sitting exactly on the source fits it with a residual of nought, and no
multiple of nought admits the beds. Over a 7.1 bed, what a bed-only fit costs
as a fraction of what the source radiates:

| source | cost |
|---|---|
| on a bed | 0.000 |
| between two beds on the floor ring | 0.061–0.077 |
| half height | 0.562 |
| overhead or high | 0.837–0.844 |

A quarter sits between them with room on both sides, and is the default.
`--overlay-beds 0` declines the preference and fits over every element.

### The guards, and why this one refuses

Everything else this encoder measures it *reports*, and a caller decides. An
overlay is different in one way that matters: it is run by a pipeline, on a
programme nobody has listened to yet, and the failure it can produce is a line
of dialogue in the wrong place or missing from a downmix. That is not a number
a log should carry quietly past someone who did not think to grep for it.

So each bound below stops the encode — `error: overlay: …` on standard error
and a non-zero exit, which a process host already turns into a failed step.
**The stream is left on disk**: a refusal is a statement about what is in the
file, and the fastest way to check a bound nobody has listened to yet is to
listen to what it stopped.

| flag | what it bounds | default |
|---|---|---|
| `--overlay-drift` | degrees between where a source asked to be and where its carriers put it | 30 |
| `--overlay-spread` | the least the strongest element carrying a source may hold | 0.5 |
| `--overlay-wobble` | per cent of windows carrying movement nobody asked for | 5 |
| `--overlay-level` | decibels a source's rendered level may stray on any presentation | 2 |
| `--overlay-cost` | the worst a source's fold may cost, as a fraction of its own gains | 0.5 |

### Why `--overlay-cost` defaults to a half, and what it assumes

A source **at a speaker's place costs exactly nothing** — it lands on that
element with a weight of one — and that is what a dub's voices are today: the
caller places them at canonical speaker positions, all of them on the floor.
So the default refuses nothing a dub currently produces, and the cases it does
refuse are sources a bed genuinely cannot reach.

**That is an assumption about the caller, and it is written down here because
it is not permanent.** The idea this mode was opened for includes one day
placing the dubbed voice on the original's *dialogue objects* rather than at
speaker places, and those can be elevated. The day a voice goes up, this bound
will refuse — which is the wanted behaviour, not a regression: the refusal
arrives with its own breakdown, and a caller who has listened can move the
bound or turn it off.

What the breakdown shows, and why it is printed as `cost of own-norm`: a cost
is a *relative* error, so it cannot be read without the vector it is relative
to. The first thing anyone suspects of a large relative error is a small
denominator. On the two positions where that was suspected, it was not the
cause — both cost most on the presentation where they radiate **most**:

| source over a 7.1.2 bed | drift | level | 2.0 | 5.1 | 7.1 | 7.1.4 |
|---|---|---|---|---|---|---|
| `[-0.623, 0.782, 0.5]` | 12.2° | +0.30 dB | 0.11 of 0.69 | 0.29 of 0.87 | 0.29 of 0.87 | **1.04 of 1.00** |
| `[-0.365, -0.931, 0.833]` | 22.8° | −0.99 dB | 0.22 of 0.56 | 0.22 of 1.19 | 0.55 of 0.84 | **0.98 of 1.00** |

The first of those is the case worth knowing about: **the drift passes and the
cost refuses, and the cost is right.** A source front-left and half-way up,
over a bed whose only height is at the sides, is built from the front floor
speaker and the side height one. Those straddle it, so their energy vector —
which is all `drift` measures — averages to nearly the right direction, while
what they *radiate* is nothing like what the source radiates. Only a layout
with front height can show it, and 7.1.4 does.

So the two guards are not redundant and do not rank the same way. Where the
cost passes, the drift passes too; the converse is false, and
`the_drift_alone_cannot_see_a_source_spread_over_the_wrong_pair` pins that
position by value so the guard is not "corrected" later by someone taking it
for a bug.

Nought turns one off — a bound chosen without a programme to choose it on is a
bound a caller who has listened knows better than. Each is refused when it is
exceeded in more than a twentieth of the blocks the source was **audible** in;
drift and the no-carrier case also when they run for more than two seconds
unbroken, because a twentieth scattered over a feature is a fault nobody
localises and a twentieth in one place is a scene. A pause ends a run: two
phrases either side of one are two runs.

Short of a refusal there are `note` lines, and what earns one differs by
bound because the bounds are not all the same shape. Drift and level remark at
**half** their bound. Wobble remarks at a **fifth** of its, since it is
already a percentage of windows. Spread remarks whenever **any** audible block
was diffuse, because its bound is a floor under the strongest weight rather
than a ceiling over an error, and half of a floor is not a milder version of
it. The no-carrier case remarks whenever it happened at all.

Every share is over the blocks a source was audible in, stranded blocks
included: a source audible with nothing to carry it is counted as placed
nowhere, so it enters the denominator of the drift share and of the fallback
share, and it dilutes the mean drift — which is stated over the same count —
without adding any angle to it. A programme stranded throughout reports a
full share, not a count over one.

`--overlay-fallback` (on by default) routes a source the fit could not place —
no carrier at all, past the drift bound, or too diffuse — onto the nearest bed
element outright. A voice on one bed is slightly misplaced and perfectly sharp,
which is the trade: audible and a little off beats diffuse and wandering. The
summary says how often that happened, because a fallback taken often is a
programme this mode is wrong for. Borrowing an inactive element for the
stretch, or falling back to a full `--cluster`, are decisions about a programme
and are the caller's.

The level guard is computed from **geometry, not audio**: what a source comes
out at on a presentation is a linear function of its weights and the elements'
positions, which `hz_cluster::metric` already reports per presentation. No
filter and no block of audio, so it is exact rather than estimated — and it
catches the case it was written for, a voice landed on an element with a small
stereo fold coefficient and vanishing from the downmix.

The wobble guard is the only one with a time axis, and so the only one that can
see the breathing at all. A dub's sources are static, so an out-and-back in
where the carriers put them is a carrier having moved under a source that did
not.

### Spare elements

A stream carries sixteen elements, and a master that brings fewer leaves room
for a source to take an element of its **own** instead of being panned —
exactly what a plain encode would have written for it, carried bit for bit
and costing nothing at all. It also makes the stream wider than the original:
a twelve-element master re-voiced with four sources would ship as sixteen.

That is not the default. **A re-voiced programme keeps the original's width**:
the stream carries exactly the elements the master brought, and every source
is panned onto them, however much room there was. `--overlay-spare` lets the
sources take the room. Whether a dub may change the original's width is a
decision about the programme, and it is asked for rather than assumed;
`a_spare_element_is_taken_only_when_asked_for` pins the default. When the
sources do take it, the ones nearest the front centre get the spares first,
which is where a dub's dialogue is; that is an angle and not a channel name,
so a master that names its channels differently still gets it right.

### What is not claimed

**None of this is measured on a programme.** The clustering's numbers do not
transfer — they are about where elements are *put*, and this puts none. What
is claimed is the arithmetic and the invariants the tests pin.

## Three decisions worth stating

**When a payload goes out.** An access unit is forty samples and object
updates are hundreds of times rarer, so restating the programme every unit
would spend bitrate on a scene that has not moved. One goes out when some
element's state changes, and otherwise every 128 units, which is how often the
reference streams measured for [fold.md](fold.md) refresh their own metadata.

**As many elements as the master has.** An object programme is any channel
count above the 7.1 bed, and shipped streams carry twelve, fourteen and
sixteen — all three measured, with the same `substream_info` and extension;
only the fourth substream's range moves. So a master's elements are coded as
they come. Padding a twelve out to sixteen, which this did at first, spends
framing on four channels carrying nothing and cost 1 % of the stream.

Nine is the fewest the shape allows, since the fourth substream has to carry
something; below that the elements are padded up with inactive ones, which is
what the syntax has the flag for. Several shipped streams park unused elements
in a corner at −114 dBFS instead; that is the same idea done less tidily, and
it cost a day of this project's measuring to see through (see fold.md).

**Where the programme starts.** Each element is seeded with its own first
update rather than with a default, so the payload written before any update has
arrived already puts everything where the master will first say it is. Starting
at the centre and jumping on the first update would be a move the master never
asked for.

## The high-resolution output timing field

A restart header carries sixteen bits of `output_timing`, which wraps every
65 536 samples. The field here carries the other sixteen, so a decoder joining
mid-stream can say where in the programme it has joined rather than only where
in the last second. It is one bit wide, so the value is serialised across many
restart headers as a run-length code, and this wrote zero in every one of them
for a while — which is not "carrying nothing" but a field a decoder starts
reading and then complains about, once every six restart headers. FFmpeg's
encoder does the same.

It is written now, and a decoder reads back what it should: `stream start
timing: 0`, then eighty-two more fields it calls valid rather than
first-of-its-kind, each checked against the last for consistency with the
access units between them. See
[`crates/hz-mlp/src/hires.rs`](../crates/hz-mlp/src/hires.rs) for the code and
the tests, which drive it through the decoder's own state machine rather than
through a second copy of the writer's assumptions.

**The preamble is written once.** Five zeroes and a one open the first field;
every field after one opens with its one alone, because a decoder that has just
read a field waits at the *end* of the preamble rather than the start of it.
Writing the five zeroes again hands it a sixth, which is the exact fault the
work was to remove — and the module's own tests called the stream correct,
because the state machine they were checking against had been transcribed with
that one line wrong. The decoder found it in a minute.

## Driving a progress bar

```bash
harlettizer encode vf.atmos --out vf.thd --cluster 12 --progress
```

One `progress <n>%` line on **standard error** each time the whole percent
changes — a hundred and one lines for any length of programme, because a bar
cannot show more than that many positions and a two-hour encode is 216 000
access units. Standard output is untouched: the summary is the command's
answer and progress is scaffolding that means nothing once it has finished, so
a caller reading both streams tells them apart by their shape.

The denominator is the input's own frame count, which both containers state in
their header, so it is a real fraction of the work rather than an estimate from
how much has been written — that would depend on how well the material
compresses, which is the one thing not known in advance. `--frames` caps it,
so a partial run still ends at 100.

The stream is byte-identical with the flag and without it.

Written for a host driving a bar, which parses these lines and nothing else.

## What `--fast` costs here

`--fast` gives the encoder one second-filter design instead of four and
re-decides it at every other restart instead of every one. On the fixture set,
run through `xtask mlp` — the encoder and nothing else — that is half the time
for about a tenth of a per cent of stream.

Through this command it is a quarter, not a half, because a master set brings
work the flag does not touch: the fold, the metadata, and reading the audio.

| | full | `--fast` | |
|---|---|---|---|
| an already-clustered master, `--cluster 12` | 13 471 476 B, 3.33 s | 13 500 570 B, 2.54 s | +0.216 % for −24 % |
| `crowd40`, `--cluster 12` | 3 733 118 B, 0.51 s | 3 784 662 B, 0.38 s | +1.38 % for −25 % |

Best of three, release, Ryzen 9 9950X. The time saving is steady across the
two; the size cost is not, and a scene of forty objects folded into twelve
pays six times what an already-clustered master does — the fold leaves the
elements less like each other, and that is exactly the material the second
filter earns its keep on.

## A fold rounds its elements, and why

`--cluster` mixes: an element is `Σ wᵢ·gᵢ·xᵢ` over the objects folded into it,
and the weights are real numbers. So every sample of every element is a product
of reals rounded into 24 bits, and the low four of those bits are noise from
the multiplication rather than signal from the mix. A lossless coder has no
choice but to carry them.

**They are expensive.** A two-hour programme, sixteen objects and an LFE folded
into twelve elements:

| | bytes | |
|---|---|---|
| every bit the mix made | 5 892 930 018 | |
| rounded to 20 bits | **4 021 429 870** | **−31.8 %** |

Same fold, same decisions — `fold 0.0006 mean, 1.416 at its worst` either way,
because rounding the output moves nothing about where the elements went. Nearly
a third of that stream was multiplication noise in the bottom four bits.

The format has the field for it: a block states an output shift and the decoder
puts the bits back, so four bits a sample a channel simply stop being written.
The encoder finds the shift itself, by looking for the bits that are zero in
every sample of the block — which is why the rounding has to land *every*
sample on the step, the ones near full scale included. Rounding one of those up
would put it past the codec's ceiling, and clamping it back to 8 388 607 —
which is not a multiple of sixteen — would cost that block all four bits for
the sake of one sample. The fold clamps to 8 388 592 instead, and
`depth_tests` pins it.

**Why this is not a loss worth minding.** Sixteen objects do not go into eleven
elements without loss — that is what a fold *is* — and a step that has already
accepted losing the difference between two directions has no principle left to
defend the difference between two values 2⁻²⁴ apart. Twenty bits puts the
rounding noise near −120 dBFS, under a programme's own noise floor and under
what a playback chain resolves.

**Where it does not apply.** Without `--cluster` nothing is mixed and nothing is
rounded: that path stays bit-exact end to end. The low frequency channel is
copied rather than mixed, so it is not rounded either.

### `--fold-depth`, and where its ends come from

```bash
harlettizer encode vf.atmos --out vf.thd --cluster 12 --fold-depth 18
```

Twenty by default. **Twenty-four** keeps every bit the mix made. **Seventeen**
is the floor, and it is the format's rather than a taste: a block declares its
dead low bits in a field capped at seven, so 24 − 7 is the coarsest grid whose
zeroes the stream can still decline to write. Round below that and the signal
is lost while the eighth dead bit is coded as a zero like any other. Both ends
are refused by name, before a sample is read.

### Match it to the source, do not guess

The number to use is a property of the *master*, not of this encoder. A
reference stream — one somebody else wrote, not anything this project did —
carries **six dead low bits on every one of its twelve channels**, at every
point sampled. It was mastered at eighteen bits.

What fills the other six is the pipeline in between. A dubbing pass separates
voices, applies gains and re-mixes in floating point, and the master it hands to
an encoder has **no dead bits at all** — twenty-four dense bits of which six are
arithmetic. On five minutes of that programme, folded to twelve elements:

| `--fold-depth` | bytes | |
|---|---|---|
| 24 | 228 421 096 | every bit the mix made |
| 20 (default) | 148 133 492 | −35.1 % |
| **18** — what the source was | **115 333 342** | **−49.5 %** |

Eighteen throws away nothing the source ever carried, and is a fifth smaller
again than the default. Twenty is the safe default precisely because it is a
guess: another master may genuinely be twenty or twenty-four, and rounding
below *its* depth loses real signal. A host that knows the source — a dubbing
tool holds the original master beside the derived one — should measure the
source's dead bits and pass the answer.

The summary says which depth was used, so a log can be read six months later.

## `--fold-search`, and the trade it names

```bash
harlettizer encode vf.atmos --out vf.thd --cluster 12 --fold-search 0
```

Four by default, which is the best fold this knows how to make. It is passes of
the search that puts each element where the fold's own cost is lowest rather
than where a clustering of directions left it — see `docs/clustering.md` for
what it does and why the earlier attempt at it lost.

Zero declines it, and that is a real choice rather than a way of turning off a
feature nobody wants — though the reason has changed since it was written, and
the change is worth recording.

**On its own, the search cost stream.** Measured against the state before any of
the clustering work landed, `crowd40` folded to twelve went from 2 704 010 B to
2 841 462 B — +5.1 % — because a better fold spreads an object over more
elements, and an element that is a denser mixture is a harder signal to code
losslessly. That is what put this behind a knob.

**Together with the rest, it does not.** Holding an object's channels between
blocks steadied the elements enough to pay for the spread, and the two effects
cancel with room to spare:

| `--fold-search` | fold, mean / worst | bytes | |
|---|---|---|---|
| 0 | 0.0597 / 0.366 | 2 925 504 | |
| 2 | 0.0545 / 0.337 | 2 849 516 | −2.6 % |
| **4** (default) | **0.0517 / 0.332** | **2 847 202** | **−2.7 %** |
| 6 | 0.0519 / 0.328 | 2 883 196 | −1.4 % |

The search now buys a seventh off what the fold costs *and* a fortieth off the
stream. Six passes is worse on both, which is the plateau showing: past four the
search is chasing a difference the coder cannot use.

The knob stays, because a measured curve with a knee is worth being able to
point at, and because the reason it was added — that a better fold might cost
bitrate — is a real mechanism that another master may well show again.

On a master that is not really folded there is nothing to gain and nothing is
spent: an eleven-object master into twelve elements costs 0.0000 and
13 464 002 B at zero passes and 0.0000 and 13 464 026 B at four.

Eight is the ceiling, and it is the wire's rather than a taste: each pass looks
half as far as the last, so past eight the search is looking finer than a
payload can say where an element is. Both ends are refused by name.

`cargo xtask cluster --search` is the same knob, so a report from the harness is
a report about a fold this command would run.

## `--drc`, and why the default is unity

```bash
harlettizer encode vf.atmos --out vf.thd --cluster 12 --drc measured
```

The stream states a dynamic range word on every substream a decoder may stop
after, restated every 128 access units. `measured` by default, a number of
decibels for a constant, `off` to state none.

Before this it stated **none**, which is what a decoder asked for compressed
output found: nothing to apply. The word had been implemented, tested and
exercised through `cargo xtask mlp --drc`; it had never been connected to the
command that makes the streams. Read back:

```text
cargo xtask thd vf.thd
  drc 0        38 of 4800 units, gain 0..0 (+0.00..+0.00 dB), refresh every 128 units
```

which is the shape a reference stream has — 282 words over 36 106 units, the
same cadence — for 304 bytes on 2.85 MB, since a word is written when it is due
and not on every unit.

`measured` is the default: each presentation's level goes through a one-pole
detector of about 700 ms and the curve read off shipped streams, and the word
is restated every 128 units. On a real master that is 283 words against its
reference stream's 282, moving 68 times against its 32.

🔴 It reads a level that is not yet the right one. This encoder's narrow
presentations are copies of the first channels rather than folds of all of
them — the presentation matrices are still to write — so the level the curve
sees is lower than what a decoder stopping there would hear, and the steeper
curve shipped streams state for a real two-channel downmix is deliberately not
applied. See [docs/drc.md](drc.md).

The summary says which gain was stated, so a log can be read six months later.

## A folding encode reads on one thread and writes on another

The fold and the coder want different things. The fold is a mix and a
clustering over a block of metadata; the coder is a filter search over a restart
interval; and neither waits on the other for anything but the samples. So the
producer reads the span, mixes it and decides its payload, one span ahead of the
thread that hands units to the encoder.

The producer owns everything it touches — the source, the fold and its running
cost, the live objects, and the count that says when a payload is due — which is
why the writing side decides none of that. Two span buffers go round between
them and are handed back to be refilled, so the whole encode holds two spans of
mixed audio rather than allocating one an iteration.

Worth 4.6 % of the wall clock. The phase timing said the fold was 11 % and it is,
measured alone; hiding it behind the coder only pays where there is a core idle
to hide it in, and by that point the coder was using most of them.

A plain encode does not go through any of this: without `--cluster` nothing is
mixed, and it keeps its own loop — a unit at a time, straight from the source.
The two had stopped being the same shape.

## The protection field, and the key that fills it

Every Evolution frame closes with an eight-bit protection field — see
[mlp.md](mlp.md#extra-data-and-the-evolution-frame-in-it) for the block it sits
in. The format calls its computation implementation dependent, no public
decoder checks it, and a decoder is told it may ignore it. What the reference
writes there is the leading byte of an **HMAC-SHA-256** over the access unit
and the frame:

- the access unit from its first byte to the start of the extra data block —
  header, major sync, directory and substreams, with the header final;
- then the frame's declared span, its padding included, with every bit from the
  first protection bit to the end of the span cleared. The four bytes between
  the two — the block's length word and the frame's own header — are left out,
  and so is the parity byte, which is restated after the field is written.

[`hz_mlp::protection`](../crates/hz-mlp/src/protection.rs) is the one place
that says which bytes, and the writer and the reader both go through it.

**The key is not part of this project.** It is read from a local settings file
that lives outside the repository — `~/.config/harlettizer/config.yaml`, or
`$XDG_CONFIG_HOME/harlettizer/config.yaml`, or whatever `$HARLETTIZER_CONFIG`
or `--config PATH` names — and nothing in the tree carries it:

```yaml
# ~/.config/harlettizer/config.yaml — this machine only, never committed.
evolution_key: "…"   # hexadecimal, 32 bytes
```

```bash
harlettizer encode programme.atmos --out programme.thd                 # the default file
harlettizer --config ~/keys/hz.yaml encode programme.atmos --out programme.thd
```

The summary says which it did:

```
  protection   each payload's frame signed with the key from /home/…/config.yaml
```

or, with no key configured:

```
  protection   no key configured, so a constant stands in the field; see docs/encode.md
```

An unkeyed stream is the same stream to the byte, but for the field and the
parity byte it disturbs; it parses identically and decodes identically, and its
field verifies against nothing. The file's shape is strict — a misspelt key is
refused rather than ignored — because a key that is silently dropped is a stream
written unsigned without anyone being told.

**Checking a stream.** `cargo xtask thd` reads the field back and, with the key,
says how many it reproduces:

```bash
cargo xtask thd programme.thd
  protection   940 of 940 fields are the digest of the key from /home/…/config.yaml
```

Run over the three reference streams to hand — 15 000, 940 and 2 824 frames —
it reproduces **18 764 of 18 764** fields. That is the whole proof that the
digest is the reference's: the framing, the truncation and the key, on streams
this project did not write. A field that does not verify is listed under
`integrity` with the unit it is in.
