# Joint object coding

Phase B3 of the lossy track (see [PLAN.md](../PLAN.md)): the parameters that
rebuild a programme's objects from the 5.1 downmix an E-AC-3 stream carries,
in the payload ETSI TS 103 420 defines. This page is the payload layer — what
is written, and how it is held to the reference streams — and the estimator
on top of it.

## The payload: `hz-joc`

`hz_joc::read` turns a payload's bytes into a model and `Joc::write` turns the
model back into bytes, both written from clause 6.2:

- **header** — the downmix configuration (five channels for 0 and 3, seven for
  1, 2 and 4), the object count, the extension configuration (only none is
  read: no stream to hand has one, and its data is not described);
- **info** — the clip gain, the ten-bit sequence count, and per object whether
  it is present and how it is coded: the parameter bands (one of 1, 3, 5, 7, 9,
  12, 15, 23), sparse or dense, fine or coarse quantisation, one or two data
  points, and for a steep slope the time slot each point takes effect at;
- **data** — per present object and data point, either every downmix channel's
  coefficient in every band (dense), or one channel and its coefficient per
  band (sparse), all through the six codebooks of annex A;
- **padding** to the byte.

The model keeps what the stream says as it says it — the codes, not the gains
they dequantise to — because what this layer has to get right is the stream;
what the codes mean is the estimator's to handle. It also keeps the three
things a decoder reads past without a word: the padding bits as they were,
any bytes after them, and the presence bits a payload of absent objects ends
before (TS 103 420 has a decoder take those objects as absent).

The codebooks are decoding trees, as the standard states them; their values
are transcribed from the workspace decoder, the annex's own tables being an
attachment rather than part of the text. `Codebook::new` walks each tree once
for the codewords an encoder needs, and the tests check each is a complete
prefix code — Kraft's sum exactly one — with every value reading back.

## Held to the reference

`cargo run -p xtask --features eac3-oracle -- eac3 <streams…>` reads every
joint object coding payload of every stream with `hz-joc`, writes it back, and
compares the bytes; and compares every field it read — header, every object's
coding, every coefficient — with what the `eac3` crate read. On the five local
streams with objects:

| Stream | Payloads | Rewritten byte for byte | Read as the oracle reads them |
|---|---|---|---|
| film, 60 s | 1875 | 1875 | 1875 |
| film, 30 s | 969 | 969 | 969 |
| sample | 1263 | 1263 | 1263 |
| channel check | 6035 | 6035 | 6035 |
| decoder fixture | 47 | 47 | 47 |

10 189 payloads, all of them. A failure fails the command.

## What the reference streams use

The same report counts how the objects are coded, and on every stream it is
one shape:

- **twelve parameter bands**, every object, every frame;
- **fine quantisation**;
- **dense** coefficients — every downmix channel in every band; sparse coding
  is never used;
- **one data point** a frame, never two;
- a **steep** slope for some objects in some frames — none in one stream, 8 to
  9 % of objects in the others — the parameters stepping at a time slot rather
  than sliding.

So that is what the estimator writes first: twelve bands, fine, dense, one
point, steep where an object moves in a frame. The sparse path and the second
data point are read and written, and round-trip through the tests, but no
reference stream exercises them — and neither has a seven-channel downmix.

## The estimator: `hz_joc::estimate`

From a frame of the 5.1 downmix and of the objects, the gains that rebuild the
objects, coded the way the references code them: twelve bands, fine, dense, one
data point, smooth.

- **The domain.** The decoder analyses the downmix with a 64-band complex QMF
  bank, applies one real gain per downmix channel to every subband of each
  parameter band, and synthesises the objects. `hz_joc::qmf` is that bank —
  the prototype transcribed from the workspace decoder, the analysis held to
  its formula by a test, the synthesis its transpose, the pair handing the
  input back 577 samples late (`qmf::DELAY`, what the decoder's objects lag
  the core by) — and the estimator fits through it.
- **The fit.** Least squares, a small ridge (10⁻⁵) keeping an empty band's
  gains at nought. The decoder slides from last frame's gains to this frame's
  across the 24 slots, and there are three ways to choose them:
  - `Fit::Synthesis`, the default, fits the decoder's *output*. Each downmix
    channel's share of each band — its subbands in that band, the others
    silent — is synthesised through the bank, sixty signals for 5.1; an
    object's sixty gains are the one least-squares mix of them nearest the
    object, delayed as the decoder delays it. The window is a frame long, Hann
    tapered, centred on frame end + 288 samples: the decoder arrives at the
    gains at the frame's last slot, and a slot's synthesis is spread over the
    prototype behind it, so that is where they are heard — not at frame end +
    577, where that sample of the core comes out (centred there, every stream
    lost 1–2 dB). The ridge is one per band, shared by that band's channels.
    One across all sixty signals flattened the quiet bands, tens of decibels
    below the loud ones: the top band missed the reference's step on 29 % of
    gains. One per signal, against that signal's own energy, did not, but it
    leaned on every channel equally whatever gain that took: of two channels
    carrying one signal, the second at −40 dB, it asked the quiet one for a
    gain of fifty, which the quantiser clamped, and rebuilt the object 4.5 dB
    low. Shared within a band, the ridge weighs every channel's gain alike and
    the fit leans on the louder. And where the fit still puts gains past the
    quantiser's −96..95 steps, the gains are solved under those bounds — the
    primal active-set method for a box: a gain is held at a bound where a step
    towards the refit crosses it first, and released again when the fit
    would gain by moving it back inside, as one can once another is held.
    Clamping gains alone, or holding every first offender for good, both
    leave mixes the quantiser could carry exactly. A test holds each case.
    On the reference streams the bounds hardly ever bind, and the figures
    below do not move with them. A frame of latency; about three times the
    centred fit's work.
  - `Fit::Centred` fits each band in the QMF domain on a frame's worth of slots
    centred on the frame's end — a frame of latency, no overshoot, and blind to
    the leakage below.
  - `Fit::Ramp` fits through the ramp from the gains the decoder holds — less
    error inside a frame, but it overshoots a change (×1.47, ×0.78, ×1.10… on a
    steady mix) and its codes grow with it, past a skip field's 511 bytes on
    one stream.
- **The steep slope.** With `Fit::Synthesis` an object's gains may *step*
  at a slot instead of ramping across the frame: the decoder holds the last
  frame's gains up to the slot and this frame's from it on (TS 103 420
  6.6.5). The references step where an object moves inside a frame — every
  object of the frame at once, at one slot spread over the whole frame, one
  data point; the gains themselves change little more than on a ramped frame
  (0.74 against 0.53 in the 60-second film, the same in the 30-second one),
  so a step says *when* the mix changes, not by how much. The estimator
  decides per object, in the QMF domain — the decoder's own model before
  synthesis — over the frame and half the next: the ramp from the held gains
  to the best gains through it, against, for each of the 24 slots, the held
  gains before it and the best gains after; all from per-slot sums, 24 small
  solves a band. The best step is taken when it leaves less than 0.7 of the
  ramp's error (`steep_margin`, 0 never steps) and buys back at least 10⁻⁴ of
  the object's energy, both weighed with the gains rounded to the
  quantiser's steps: a gain between two steps looks better reached at once
  than ramped to until both round to the step already held, and stepping on
  that kept a steady mix stepping every frame. A stepped object's gains are
  then fitted by synthesis on the part of the window only the slots after
  the step make, and held to the same test the step passed: that part can
  hold nothing to fit — a transient over before it begins — and the refit
  come back nought, which erased the transient (a test holds the case). The
  gains that fail are replaced by the decision's own, fitted in the QMF
  domain on every slot from the step on; on the 60-second film that also
  gained 0.15 dB on the slid frames.
- **The budget.** A skip field holds 511 bytes, and the payload shares them
  with the frame's object metadata and the rest; the caller sets
  `Estimator::budget` to what is left. A payload over it has every present
  object's codes chosen again, channel by channel, for the least error plus
  λ times their bits — a Viterbi search over each band's candidate levels, a
  few either side of its own, the channel's weighted mean and nought — one λ
  for the frame, the least that fits: bracketed up or down from a scale set
  by the objects being quantised — an absent object's last weights once
  set it, and a quiet frame's present object was sent at nought — then
  found by bisection of the ratio. The error is
  weighed by the fit's own normal equations: the synthesis fit's diagonal,
  the centred fit's per band. Codes are differences between neighbouring
  bands and a difference of nought costs one bit, so a larger λ flattens the
  gains across bands where that costs least; in the limit every code is
  nought, the smallest a payload of the frame's objects can be, which is sent
  if even it does not fit. A payload under its budget is untouched, byte for
  byte, and only a frame over it pays for the search.
- **The state.** It keeps what the decoder holds: the dequantised gains of the
  last frame an object was present, untouched while it is absent, as the
  decoder leaves them; and the sequence count, which after 1023 restarts at
  *one* — nought marks a splice, and a decoder that sees it drops everything.
  (The first version wrapped to nought, and every 1024th frame of the channel
  check glitched: 35.4 dB where 54.3 is the real figure.)

### Against the reference

`cargo run -p xtask --features eac3-oracle -- graft reference.ec3 --out
refit.ec3 --refit synthesis` (or `centred`, `ramp`) decodes a reference stream's core and the objects it
rebuilds, estimates the gains from them, carries them in place of the
reference's on the core `hz-ac3` codes again, and decodes the objects back. It
reports how close the gains come to the reference's, and the objects rebuilt
from them beside the objects the reference's own gains rebuild on the same
core, split between the frames the reference slid its gains in and those it
stepped them in, the stream's first two and last two frames left out.

*The same core* is enforced, not assumed. A skip field's bytes come out of the
audio's budget, so two encodes carrying payloads of different sizes code the
audio differently — 204 bytes against 207 left 16 % of a synthetic core's
samples changed. Both encodes therefore carry each frame's Evolution frame
padded with zeros to the larger of the two (the container's length may run
past its last field, TS 102 366 H.2.2.1.2), and the command refuses to report
unless the two decoded cores are identical. And where the reference's sequence
count breaks — a splice, after which its decoder starts over — the fit starts
over too: each unspliced run of frames is fitted by an estimator of its own,
numbered from the reference's count, its window never reaching across the cut,
and the core it reads is decoded again from nothing at the cut, as the
reference's objects are — a test fits a segment alone and after a splice and
asks for the same payloads.
The sample's first frame is such a run, a leftover of the cut it was taken
from.

| Stream | Fit | Gains on the reference's step | Payload, bytes, mean / max (theirs) | Objects, slid frames: ours / theirs | Stepped frames: ours / theirs |
|---|---|---|---|---|---|
| film, 60 s | synthesis, steep | 96.8 % | 209 / 327 (207 / 311) | 30.3 / 35.5 dB | 25.7 / 34.5 dB |
| | synthesis | 96.9 % | 209 / 336 | 30.0 / 35.5 dB | 22.5 / 34.5 dB |
| | centred | 92.3 % | 204 / 303 | 26.6 / 35.5 dB | 21.7 / 34.5 dB |
| film, 30 s | synthesis, steep | 98.9 % | 170 / 201 (170 / 202) | 37.3 / 41.7 dB | 32.9 / 38.2 dB |
| | synthesis | 98.9 % | 170 / 267 | 36.8 / 41.7 dB | 28.7 / 38.2 dB |
| | centred | 96.5 % | 167 / 247 | 33.2 / 41.7 dB | 28.4 / 38.2 dB |
| sample, two runs | synthesis, steep | 93.7 % | 189 / 372 (181 / 218) | 30.7 / 34.6 dB | — |
| | synthesis | 93.6 % | 189 / 371 | 30.7 / 34.6 dB | — |
| | centred | 86.3 % | 185 / 354 | 27.0 / 34.7 dB | — |
| channel check | synthesis, steep | 99.9 % | 133 / 165 (134 / 145) | 57.1 / 60.7 dB | 58.3 / 58.9 dB |
| | synthesis | 99.9 % | 133 / 153 | 56.7 / 60.7 dB | 57.2 / 58.9 dB |
| | centred | 99.8 % | 134 / 157 | 54.3 / 60.7 dB | 56.8 / 58.9 dB |

The comparison favours the reference by construction: its objects *are* what
its gains rebuild, so its gains reproduce them up to the core's coding, where
any other quantisation of the same mix pays the full step — a tenth of a gain,
some −20 dB in that band for a gain near one. What the table measures is how
nearly an estimate recovers a known mix.

The centred fit, band by band in the QMF domain, missed the reference's step
on film mostly in the **narrow low bands** — one subband each up to band 3 —
at two to five times the rate of the wide bands. That is the subbands'
leakage: the decoder's synthesis lets each subband carry some of its
neighbours', which in a band one subband wide is a large share of the band,
and a fit made band by band cannot see it. Fitting the output, all of an
object's bands together, sees it:

```
off the reference's step, by band (film, 60 s)
  centred:    9.3 16.5 12.5 13.0 8.8 7.8 5.1 4.9 4.4 3.0 2.7 4.8 %
  synthesis:  3.5  4.8  5.3  6.2 4.2 2.9 2.2 1.9 1.6 1.5 1.3 2.3 %
```

The gain is the synthesis and not the window: the centred fit at its best
window (six to eight slots either side) reaches 27.7 dB on the sample and 34.4
on the 30-second film, three to four short. The window's shape was measured
too: a flat one a frame long fits each frame's gains partly to its neighbours'
mixes (27.4 dB on the sample); a flat one eight slots long recovers that
(31.7) but chases transients, one frame of the 60-second film's payload
reaching 439 bytes and its Evolution frame 537, past the skip field's 511;
pulling the ridge towards the held gains or smoothing across bands brings it
under only at a loss of 1 to 3 dB. The Hann window keeps most of the
short window's gain without the spikes, and is the one that beats the centred
fit on every stream, slid and stepped.

The steep slope closes part of the stepped frames' gap: 3 to 4 dB on film,
and the whole stream's objects gain 1.2 dB on the 60-second film, 3.6 on the
30-second one (28.1 → 29.4 and 32.9 → 36.6 dB). Where it steps, the reference
mostly does too, at the same slot to within one: on 3.8 of the 6.0 % of
objects' frames the 60-second film steps, 1.2 of 1.4 % on the 30-second one;
on the sample, which never steps, it steps 1.0 % at no cost to the slid
frames (30.65 against 30.68 dB). It steps less than the reference — 6.0
against 9.1 %, 1.4 against 7.8 % — because the reference steps every object
of a frame together, where the estimator steps an object only when its own
error says so; stepping more, with a looser margin, gained a little on the
stepped frames and lost it on the slid ones (measured before the gains sent
were held to the step's test, which moved the 60-second film by 0.15 dB and
nothing else):

| Margin | Steps (film 60 s / 30 s) | Objects, whole stream (60 s / 30 s / sample) |
|---|---|---|
| 0 (never) | — | 28.1 / 32.9 / 29.3 dB |
| 0.7 | 6.1 / 1.4 % | 29.3 / 36.6 / 30.4 dB |
| 0.8 | 7.8 / 1.8 % | 29.3 / 36.7 / 30.4 dB |
| 0.9 | 10.3 / 2.3 % | 29.2 / 36.8 / 30.3 dB |

The decision makes the fit about 30 % slower; the whole graft still runs
some eight times faster than real time.

`graft` gives the estimator each frame's room — the skip field's 511 bytes
less the frame's other payloads and the container, the payload's size field
at its longest — and `--skip-limit` lowers the 511 to exercise the budget.
At 511 no payload of the four streams needed it, and every figure above
stands. Lower, the 60-second film, whose payloads run largest:

| Skip limit | Payloads made smaller | Payload, mean / max | Objects, whole stream |
|---|---|---|---|
| 511 | 0 | 209 / 327 bytes | 29.40 dB |
| 400 | 1 | 209 / 302 bytes | 29.40 dB |
| 330 | 387 of 1875 | 206 / 232 bytes | 29.38 dB |
| 280 | 1635 of 1875 | 180 / 182 bytes | 28.05 dB |

At 280 the room falls below the payloads' mean, and most frames give up a
seventh of their bits for 1.35 dB; the sample and the 30-second film, at the
same limit, make 4 and 17 payloads smaller and lose nothing measurable. A
frame made smaller takes some 8 ms more, a quarter of its own duration.

## Objects of our own: `harlettizer ac3 --eac3 --joc`

From a master, the 5.1 core is the programme rendered and folded as
`harlettizer ac3` renders it, and every object and bed channel but the LFE is
an element of its own — fifteen at most, what the reference streams carry; a
master with more has them folded into fifteen (below). Each syncframe
carries an Evolution frame in block 2's skip field with the payloads the
references carry, in their order and configured as theirs are:

- the programme's loudness (payload 1, for the frame's 1536 samples, kept):
  every sixteenth frame the core's true peak since the last statement, as the
  references state theirs — the interval closed only once the meter's
  reconstruction, six samples behind, has caught up with its end, or a
  transient there is counted in the next — and nothing else — no measurement practice is
  claimed, and no integrated loudness, which has to match the stream's
  dialnorm, and the dialnorm is the user's (`--dialnorm`), not measured;
- the programme information (payload 2, discarded by a decoder that does not
  know it): how the core was folded down from the 7.1 render — the spread
  fold, each rear surround on its own side at −1,25 dB and the far at −6,0,
  is table H.3.9's type 2, which a test holds to the fold's coefficients —
  nothing else done to the audio — no surround attenuation, no 90° phase
  shift, no LFE filter — and no compression profile, since the stream
  carries no compression words;
- the object metadata (payload 11, one block, every element moving across the
  frame to where it is at the frame's end — the references' `Long` ramp);
- the joint object coding payload (14).

The last two are in group nought and kept, the joint object coding
frame-aligned. `hz_meta::programme` reads and writes the first two, and
`cargo xtask eac3` holds it to every one the reference streams carry —
20 284 payloads, each written back byte for byte — and fails when one is not.

The metadata is timed on the clock of the objects a decoder plays, not the
core's: they come out the core's 256 samples and joint object coding's 577
behind the master, and the metadata comes with the access unit that carries
them, so a frame's positions are the master's 833 samples before its end.
Timed on the core's clock, as the first version was, a moving object was
placed 577 samples ahead of its sound. Each object is rendered as the master
says — its size, and whether it snaps, which zones confine it, how it follows
the screen and whether its height counts, mapped as the TrueHD writer maps
them — where the first version reset them all. And the stream runs until the
objects are out, the 833 samples covered, not only the core's 256: a
programme of 1280 samples takes two syncframes, where one lost its last 577
to the objects.

The gains are fitted to the core a decoder plays, which `hz-ac3` reconstructs
(docs/ac3.md). Two things are in the way of fitting it and carrying the result
in the same frame: a frame's fit looks half a frame ahead, into a core not yet
coded, and the Evolution frame's bytes come out of the audio's, so what it
carries changes the core it was fitted to. So every Evolution frame is padded
to one size — enough for the references' payloads, sixteen bytes an element,
and never less than the smallest payload the elements can make — which makes
the core's bits independent of what it carries; and two encoders run, a probe a
frame ahead carrying a placeholder of that size, whose reconstruction the gains
are fitted to, and the stream's encoder a frame behind carrying the real
thing. The two code the same audio into the same bits, and every frame checks
that they did: the stream's core is the core the gains were fitted to, or
nothing is written. (A test gives the probe a placeholder forty bytes short,
and the writer refuses its first frame.)

`hz-meta`'s object metadata writer reads and writes back every payload of the
30-second film byte for byte (`cargo xtask eac3` now reports it), whose
programme is the same shape as ours: the LFE, then dynamic objects, no sample
offset. The other references use what it does not model — a sample offset
inside the payload, speaker-anchored beds — which nothing here writes.

`cargo xtask joc-check stream.ec3 --master in.atmos` decodes a stream's
objects through the workspace decoder and holds each to its element of the
master, the decoder's lag — the encoder's delay and joint object coding's 577
samples — taken out:

| Master | Elements | Evolution frame | Objects against the master's elements |
|---|---|---|---|
| synthetic, 4 objects and a bed channel | 5 | 133 bytes | 26.3 dB (47.7, 30.8, 24.1, 22.5, 28.4) |
| synthetic, 10 broadband objects | 10 | 233 bytes | 7.1 dB |
| synthetic, 10 objects switching on and off | 10 | 233 bytes | 9.9 dB |
| synthetic, a 7.1.2 bed and 6 broadband objects | 15 | 333 bytes | 7.2 dB |

The workspace decoder reads every frame's object metadata with the elements
the master has, and the LFE. These are not the reference comparisons above:
those hold the gains to what a reference's own gains rebuild, which is itself
a joint object coding reconstruction; these hold the objects to the master's,
which no five-channel downmix can give back where ten or fifteen independent
broadband sources overlap in the same speakers. Room is not what limits them:
an allowance of 16, 24, 32 or 64 bytes an element moves the result by less
than half a decibel, the larger frames taking their bytes from the core.

## Folding a larger master

A master with more than fifteen elements — real ones bring forty and more
objects — has its objects and bed channels folded into fifteen by the same
clustering the TrueHD and IAMF writers fold theirs with:
`hz_programme::fold::Folder`, which `iamf --objects` now drives too (its
output is byte for byte what it was). The core does not change: it is still
the master's whole render, folded to 5.1. The fold decides what the stream
carries as objects, and where the object metadata puts them.

- **A block a syncframe, on the decoded objects' clock.** Block *k* is the
  master's samples from *k*·1536 − 833 to (*k*+1)·1536 − 833. That is the
  span frame *k*'s objects come out of a decoder as, and the span its object
  metadata ramps their positions across (above). So the weights ramp across
  the same span to the same positions a decoder's do. 1536 is a little
  longer than the TrueHD encode's 1280; the IAMF writer already takes any
  block up to 1280 that divides its unit.
- **Two clocks.** The estimator takes a frame's elements on the core's
  clock, 256 samples behind the master: 577 samples into the frame's block
  and on into the next one. So a frame is coded once the block after it is
  folded, and the last frame's elements run on into silence.
- **The window ahead.** Each block is placed for the energies of the two
  blocks after it (`hz_cluster::smooth::SMOOTHING`). So the fold holds three
  frames back in all, each with the core rendered beside it, and the writer
  drains them at the end. The stream still has a syncframe for every frame
  that went in.
- **The bed channels are free.** Pinning them would keep an element each, and
  a 7.1.2 bed's nine would take most of the fifteen. The core already carries
  them where they are. That is what `encode --beds auto` decides whenever a
  fold is needed at all.
- **Nothing is rounded or limited.** The elements are never coded as samples,
  only rebuilt from the core, which is clamped on its own; a limiter would
  pull them away from the scene the core was rendered from. The fold's floor
  is the stream's dialnorm.
- **Rendering classes are kept apart**, as the TrueHD fold keeps them
  (`hz_cluster::class`): objects that snap, or are confined to zones, or
  follow the screen, share an element only with their own kind. Each
  element's metadata states its class's rendering. An object's size does
  not survive the fold, as it does not in the TrueHD encode: an element is
  a point.

`HZ_JOC_TRACE=<dir>` writes what the stream was meant to carry:
`element-<n>.f32`, each element's samples on the master's clock, and
`positions.csv`, where each element is at the end of each frame's block, as
the metadata codes it. `joc-check --elements <dir>` holds the decoded objects
to those elements, not to the master's, and renders the scene on 7.1.4 three
ways: the master's render (bed channels at their speakers, objects on the
room's cube), the carried elements where the metadata puts them, and the
decoded objects where the metadata puts them. The last two are held to the
first. The first number is what the fold and the metadata's one position a
frame cost; the second is what the stream costs in all.

On the 40-object scenes and on `make-master --objects 30 --bed 7.1.2`
(8 seconds), each master's elements folded into fifteen:

| Master | Elements | Fold, mean / worst | Objects against the carried elements | Scene: carried elements | Scene: decoded objects |
|---|---|---|---|---|---|
| crowd40 | 40 → 15 | 0.029 / 0.31 | 7.1 dB | 23.2 dB | 11.6 dB |
| broad40 | 40 → 15 | 0.018 / 0.30 | 3.3 dB | 23.5 dB | 7.5 dB |
| bed712_40 | 49 → 15 | 0.061 / 0.85 | 4.6 dB | 16.2 dB | 6.4 dB |
| class40, every third object snapping | 40 → 15 | 0.068 / 1.41 | 3.3 dB | 17.6 dB | 7.4 dB |
| 30 tones, 7.1.2 bed | 39 → 15 | 0.071 / 0.79 | 8.1 dB | 16.6 dB | 9.7 dB |
| 30 broadband, 7.1.2 bed | 39 → 15 | 0.046 / 1.15 | 5.1 dB | 18.3 dB | 6.6 dB |
| 30 bursts, 7.1.2 bed | 39 → 15 | 0.030 / 1.13 | 6.1 dB | 20.1 dB | 7.2 dB |
| *as they are, for scale:* | | | | | |
| 10 objects switching on and off | 10 | — | 9.9 dB | 26.9 dB | 12.5 dB |
| a 7.1.2 bed and 6 broadband objects | 15 | — | 7.3 dB | 31.5 dB | 8.1 dB |

Every master decodes with fifteen objects and the LFE in every frame's
metadata. These synthetic masters are loud: their 5.1 renders peak at 2.9 to
5.4 times full scale, and the core is clamped. Copies 18 dB down, which clip
nothing, move the objects by +0.1 to +1.6 dB and the decoded scene by +0.4 to
+1.8 dB. So the clipping is not what holds them down. The density is:
fifteen broadband elements rebuilt from five channels. The decoded scenes of
the folded masters, 6 to 12 dB, are where a fifteen-element master carried as
it is lands (the last row, 8.1 dB). The fold's own cost is the TrueHD
encode's at the same width: crowd40 into fifteen elements beside the LFE
costs 0.026 on 1280-sample blocks and 0.029 here. On broad40 the two do not
compare: the TrueHD fold weighs the master's object sizes, and this one, like
IAMF's, judges points.

## Not done

- Two data points a frame, which the references never use, and a steep slope
  decided for the frame's objects together, as they do.
- Coarse quantisation and fewer parameter bands, which the budget could also
  trade for bits; the references use neither, and at a skip field's 511
  bytes nothing yet needs them.
- A fold shaped for joint object coding: the clustering places elements for
  a listener, by the metric's presentations, not for how well five channels
  give each one back. And fewer than fifteen elements, when fewer would
  rebuild better, which nothing here measures yet.
- An element's size in a fold, which the TrueHD encode does not state
  either.
- The programme's integrated loudness, gated on level and on speech, which
  the references state every thirty-two frames: it has to match the stream's
  dialnorm, so it waits on a pass that measures the programme before coding
  and on how the dialnorm is then set. And listening, on a receiver.
- The extension configuration's data.
