# Folding an object onto a channel presentation

A stream carries several presentations of one programme, and only the widest is
authored. The narrow ones are built by folding: every object has to arrive
somewhere in a layout that has no place for it. [`hz-render`'s
`ObjectFold`](../crates/hz-render/src/fold.rs) is that fold, and this is where
its coefficients come from.

## It was measured, not designed

The obvious implementation is the one already in
[`panner`](../crates/hz-render/src/panner.rs): vector-base amplitude panning
over a triangulated hull. It is a good renderer. It is not what a reference
stream's presentations are made with, and we can say that with numbers because
a shipped stream contains both halves of the answer.

`harletty decode --presentation 3` gives the object elements. `--presentation 2`
gives the encoder's own 7.1 fold of exactly those elements. Both come out of the
same access units, so solving for the matrix between them in a window short
enough that nothing moves recovers the gains an encoder under a real licence
gave one object at one position. No encoder is run to get this; a shipped
stream is decoded, which is what the rest of this project already does to pin
every field it writes. See [LEGAL.md](../LEGAL.md) §3.1 for why that
distinction is the whole of it.

**Source.** A reference stream, called A below: twelve 120-second slices
spread across the programme — 13 506 position updates, fifteen objects, every
height code from 0 to 1. Most streams are not usable for this: three others
tried carry a static 7.1.2 bed and no moving object at all in the slices
sampled. Check the metadata for movement before measuring anything.

## What it turned out to be

Separable, equal power, and linear in the room's own coordinates.

```
rows        front   y = +1   L(x = -1), C(x = 0), R(x = +1)     spacing 1
            side    y =  0   Ls(-1), Rs(+1)                     spacing 2
            rear    y = -1   Lrs(-1), Rrs(+1)                   spacing 2

depth(row)  = cos(pi/2 * clamp(|y - y_row|, 0, 1))          normalised over rows
width(spk)  = cos(pi/2 * clamp(|x - x_spk| / spacing, 0, 1)) normalised in the row
gain(spk)   = depth(row of spk) * width(spk)
gains       = k(z) * gains / |gains|
```

`z` does not steer. It scales, and nothing else: over 111 pairs of measurements
that share `(x, y)` at different heights, the cosine between the two normalised
gain vectors is **0.9988** on average and 0.957 at worst.

### The speakers are at room corners, not at their azimuths

Front left sits at `x = -1`. Projecting its nominal 30 degrees onto the cube
would put it at `-0.577`, and a pan built on that is wrong everywhere except at
the speakers. The positions used are the ones a stream's own bed elements
declare for themselves, which is also how they were found: inferring each output
channel's direction from the measured gains alone gives -45, 0, ±88 and ±135
degrees, and never anything for the LFE.

### The spacing is per row

The front row holds three speakers one unit apart, the side and rear rows hold
two, two units apart. Dividing by a single radius instead makes an object at
`x = 0` in the side row score `cos(pi/2) = 0` in *both* speakers instead of
`0.707` in each; the vector is renormalised afterwards and still looks like a
pan. This one mistake cost most of a day, and it fails only *between* speakers —
exactly where a spot check does not look. Fixing it moved elevated objects from
0.285 error and nothing within tolerance to **0.051 and 93%**.

### The height scale

| z | 0.00 | 0.20 | 0.27 | 0.33 | 0.40 | 0.47 | 0.53 | 0.60 | 0.67 | ≥0.73 |
|---|---|---|---|---|---|---|---|---|---|---|
| k | 0.9487 | 0.9470 | 0.9445 | 0.9295 | 0.8926 | 0.8849 | 0.8610 | 0.8514 | 0.8431 | 0.8418 |

Flat near the floor, falling through the middle, flat again from two thirds up.
`k(0)` is `sqrt(0.9)` to four decimal places and the plateau is −1.495 dB, but
neither closed form is claimed: the table is what was measured, and
`elevation_scale` interpolates between measurements rather than through a curve
nobody fitted.

**And the curve is not universal.** A third stream, C, gives a flat
**0.8417 ± 0.003** at every height it visits — the value this table reaches
only at its top — where the table would say 0.9487 at the floor and 0.9295 at
`z = 0.33`. Everything else about it matches: the direction of its gain vectors
agrees with the law at a cosine of **0.99992**, fifth percentile 0.99991, and
substituting the flat constant takes the error against it from 0.1234 to
**0.0149**, with 99.7% of objects inside 0.05 and 97.9% inside 0.02.

That splits the law in two. The **direction** looks like a property of the
format — three streams now, agreeing to five decimal places on a quantity that
has eight degrees of freedom. The **scale** looks like a property of the
stream, as the fold matrix already did. Three streams is not a taxonomy, and
`elevation_scale` still carries the two-stream curve; a job transcoding
content like C would need the constant instead.

## Bed elements are routed, not folded

An element parked on a speaker position does not go through this at all. The
reference gives those fixed gains — unity on the front row and the LFE, `0.8665`
on the sides, `0.8419` on the rears, identical across five streams. Scoring them
as though they had been panned is what makes a correct fold look broken:
including them drops the floor score from 85% to 13%.

## What it scores

Against 186 922 measurements, elements parked on a speaker excluded:

| | error / signal | within 0.05 on every channel | within 0.02 |
|---|---|---|---|
| floor, `z = 0` | 0.0568 | 84.9% | 51.5% |
| elevated | 0.0555 | 92.5% | 76.7% |
| **all** | **0.0562** | **88.8%** | **64.3%** |

600 of those measurements, drawn uniformly, are checked into
`crates/hz-render/tests/data/fold_samples.csv` and scored by
`crates/hz-render/tests/fold.rs` on every run. Drawn *uniformly*: a stratified
sample over-weights the sparse corners of the room and reports 69% where the
population gives 89%.

### Two traps in measuring this at all

**The decoder's CAF is big-endian.** `truehdd`'s `.atmos.audio` carries `flags =
0x0` in its `desc` chunk, and bit 1 is `kCAFLinearPCMFormatFlagIsLittleEndian` —
absent means big. Read it the other way and the channels still look like audio,
with lag-1 autocorrelation from 0.27 to 0.94, but correlate with nothing. That
looks exactly like a broken decoder and is not one.

**Filter on the fit's own uncertainty, not on energy.** Selecting windows where
one object is loud selects objects near speakers, and the law then scores 90%
where an unbiased sample gives 17% — which is how the spacing bug stayed hidden.
The harvest keeps a coefficient when its standard error from the least-squares
covariance, `sqrt(diag((XᵀX)⁻¹) · σ²)`, is small enough, and tightening that
threshold does not improve the score, which is how we know what is left is
model error rather than noise.

## 5.1 and 2.0 are this render, then a matrix

Not a render on their own geometry: putting the object straight onto a 5.1
layout scores 0.143 where taking the 7.1 gains through a fixed matrix scores
0.060. So `ObjectFold` always renders at 7.1 and folds from there.

**And there are two matrices, not one.** Each stream holds to one of them
across both its narrow presentations, so it is a declaration rather than a
drift, and they are nowhere near each other: applied to the wrong stream either
one leaves about 0.25 of the signal.

| | rear surround goes to | stereo fold |
|---|---|---|
| `FoldMode::Spread` | both sides, at `(sqrt(3)/2, 1/2)` | `0.52 · (L + 0.707·C + 0.909·Ls)` |
| `FoldMode::SameSide` | its own side, at unity | `0.5 · (L + 0.707·C + Ls)` |

The third stream, C, does neither: it mixes the **side** surrounds into each
other as well, `(1.082, 0.230)`, and sends the rears in at `(0.753, 0.160)` —
the same left-to-right ratio, 0.2124 against 0.2122, in both rows. Its stereo
fold is the published one applied on top of that, `0.5012` scale with centre at
`0.7079`. It is measured over 275 restart intervals and is not implemented:
one stream is an observation, not a mode.

`SameSide` is the published fold. `Spread` is not: ATSC A/52 sends a rear to one
side at half power, whose squares sum to 0.5 where these sum to 1. Where the
reference spreads, it preserves power, as it does everywhere else in this fold.

| stream | presentation | mode | error / signal | within 0.05 | within 0.02 |
|---|---|---|---|---|---|
| A | 7.1 | — | 0.0553 | 88.8% | 65.2% |
| A | 5.1 | Spread | 0.0507 | 88.2% | 66.3% |
| A | 2.0 | Spread | 0.0500 | 94.8% | 77.5% |
| B | 7.1 | — | 0.0133 | 99.6% | 98.7% |
| B | 5.1 | SameSide | 0.0082 | 99.9% | 98.9% |
| B | 2.0 | SameSide | 0.0190 | 99.6% | 99.0% |

The second stream, B, fits far more tightly than the first, and the reason is
upstream of the matrix: the same 7.1 render, unchanged, scores 0.013 there
against 0.055 on A. That is the best cross-stream evidence the render law has
— it was fitted on one stream and transfers to another at 98.7% within 0.02.

### Which mode a stream is in, and why the header does not say

Nine streams were measured, by reading the rear rule straight off their bed
elements and, where a stream parks none, by fitting both presentations onto the
elements in the same window and solving one against the other — neither method
knows the panning law:

| stream | `twoch_control_enabled` | fold |
|---|---|---|
| A | 0 | Spread |
| D | 0 | Spread |
| E | 0 | Spread |
| F | 0 | Spread |
| G | 0 | Spread |
| H | 0 | Spread |
| I | 0 | Spread |
| J | 0 | **SameSide** |
| K | 0 | **SameSide** |
| C | 0 | **a third form** |
| B | 1 | SameSide |
| L | 1 | SameSide |
| M | 1 | SameSide |

**The bit does not predict the fold.** Set, it has coincided with `SameSide`
three times out of three; clear, it gives all three behaviours. Seven of the
ten clear streams spread, two fold to their own side and one does the third
thing, so knowing the bit is clear tells you nothing. Whatever the one-way
reading is worth rests on three streams.

Every other field of the channel meaning is **identical across all nine** —
2ch, 6ch and 8ch dialogue norms, mix levels and source formats, and the two
remaining control bits. `twoch_control_enabled` is the only structural field
that varies, and on the first nine streams it lined up eight times. Four more
streams took that apart: it now fails twice among the ten that have it clear,
and both failures read clean over hundreds of restart intervals. One of them,
J, was at first suspected of being a re-encode; it is not: once its two silent
elements are kept out of the solve its fold is unambiguous.

One correlation with one counterexample is not a bit, and an earlier pass here
claimed more than that from three streams — it read `02 32 ca` against
`00 37 4a` as a difference in the presentation fields, when the byte that
looked decisive is the low bit of `drc_start_up_gain`, a per-stream number.

### It is a property of the stream, not of the moment

A narrow presentation is the earlier substreams' matrices, and those are
restated every restart interval — 5120 samples — so nothing in the format stops
a stream changing its mind partway through. None do. Four streams were sampled
at six points spread evenly across each runtime, and within every 30-second
window the fold was solved **per restart interval**:

| stream | points | intervals | Spread | SameSide |
|---|---|---|---|---|
| A | 6 | 1 254 | 1 254 | 0 |
| E | 6 | 1 461 | 1 461 | 0 |
| B | 6 | 1 702 | 0 | 1 702 |
| J | 5 | 1 287 | 0 | 1 287 |

Not one interval of ~5 700 disagreed with its stream, and the coefficients
themselves hold to ±0.007. So `FoldMode` is rightly a property of the fold
rather than something to re-read as a job runs. Six 30-second windows is about
three minutes of a two-hour programme, so what this rules out is drift, not a
single deliberate change at a reel boundary.

### Drop the silent elements before solving anything

Stream J looked broken for a while: a tenth of the usable intervals of any
other stream, and its own 7.1 and 5.1 leaving 0.197 of signal when solved
against each other where the rest leave 0.002. It is not broken. Two of its
elements sit at −114 dBFS and are bit-identical to each other in 99.5% of
samples — the element set is rank-deficient, the least squares cannot split
those two columns, and it puts arbitrary values there that differ between one
presentation and the next. The whole solve inherits the difference.

The energy floor that let them through was `sum(x^2) > 1e6` over 1.4 M samples,
which is an RMS of 0.83 out of 8 388 608. Raising it to an RMS of 100 takes J
from 119 usable intervals to 1 287, every one of them agreeing with
every other, and its numbers become the tightest in the table. A threshold that
admits the noise floor does not merely add noise; it adds *unconstrained
parameters*, and those are worse.

The likelier answer is that nothing declares it, because nothing needs to. A
narrow presentation *is* the earlier substreams' matrices; those are in the
bitstream, and a decoder applies them without being told what they mean. An
encoder picks, both picks ship, and this one picks too — explicitly, through
`FoldMode`.

### Measured directly, without the renderer in between

The rear rule can be read straight off the bed elements, which are routed
rather than folded, so nothing is fitted:

| stream | rear-left bed element → Ls / Rs |
|---|---|
| A | 0.872 / 0.490 |
| D | 0.872 / 0.490 |
| B | 1.000 / 0.000 |

All three give the rear the same magnitude, 0.841. Only where it goes differs.

### Which stream you measure this on decides the answer

The major sync says whether the narrow presentations are derived from the
elements at all. Fit on the first half of a restart interval, predict the
second:

| streams | `substream_info` / extended | 7.1 | 5.1 | 2.0 |
|---|---|---|---|---|
| A, D, B | **0xfc / 3** | 0.027 | 0.029 | 0.019 |
| three others | **0xcc / 1** | 0.001 | 0.89–1.85 | 0.89–2.35 |

In `0xcc`/`1` streams the 5.1 and 2.0 presentations are **separately authored**
and no fold reproduces them, while the 7.1 is derived as usual. Measuring on
one of those is what made this look unsolvable for two rounds: the first
stream tried was one of them.

### There is no 58-sample delay

An earlier pass here reported one, from cross-correlating a narrow presentation
against the 7.1. There is not: applying it takes the held-out error from 0.03 to
5.13. Two different mixes of one programme correlate at whatever lag their
loudest content happens to line up on, and that lag wanders — 47, 59, 61, 383,
394 samples across one slice. A wandering delay is not a delay.

## What is not here

**Height speakers.** `ObjectFold::for_layout` refuses 7.1.4 rather than guessing
what the reference does with `U+030`, because nothing was measured there:
stream A's presentations have no height layer to read one off.
