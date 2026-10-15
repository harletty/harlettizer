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
    the fit leans on the louder. And a gain the fit still puts past the
    quantiser's −96..95 steps is held at the bound and the others are fitted
    again around it, rather than clamped alone. A test holds each case. A
    frame of latency; about three times the centred fit's work.
  - `Fit::Centred` fits each band in the QMF domain on a frame's worth of slots
    centred on the frame's end — a frame of latency, no overshoot, and blind to
    the leakage below.
  - `Fit::Ramp` fits through the ramp from the gains the decoder holds — less
    error inside a frame, but it overshoots a change (×1.47, ×0.78, ×1.10… on a
    steady mix) and its codes grow with it, past a skip field's 511 bytes on
    one stream.
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
| film, 60 s | synthesis | 96.9 % | 209 / 336 (207 / 311) | 30.0 / 35.5 dB | 22.5 / 34.5 dB |
| | centred | 92.3 % | 204 / 303 | 26.6 / 35.5 dB | 21.7 / 34.5 dB |
| film, 30 s | synthesis | 98.9 % | 170 / 267 (170 / 202) | 36.8 / 41.7 dB | 28.7 / 38.2 dB |
| | centred | 96.5 % | 167 / 247 | 33.2 / 41.7 dB | 28.4 / 38.2 dB |
| sample, two runs | synthesis | 93.6 % | 189 / 371 (181 / 218) | 30.7 / 34.6 dB | — |
| | centred | 86.3 % | 185 / 354 | 27.0 / 34.7 dB | — |
| channel check | synthesis | 99.9 % | 133 / 153 (134 / 145) | 56.7 / 60.7 dB | 57.2 / 58.9 dB |
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

What is left is mostly the stepped frames, where the reference moves its gains
within the frame and ours, one set a frame, cannot follow — the steep slope.

## Not done

- The steep slope: the stepped frames above.
- A budget for the payload's size: ours run near the reference's, its largest
  within a quarter of theirs on film, but nothing holds them under a skip
  field's 511 bytes.
- The downmix: these gains are fitted to a reference's core. From a master, the
  core is the objects rendered to 5.1, then coded, and the gains fitted to the
  decoded core — the step that makes `harlettizer` write objects of its own.
- The extension configuration's data.
