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
  parameter band, and synthesises the objects. `hz_joc::qmf` is that analysis
  bank — the prototype transcribed from the workspace decoder, the modulation
  held to its formula by a test — and the estimator fits in it.
- **The fit.** Per object and band, least squares of the object's subbands from
  the downmix's, a small ridge (10⁻⁵ of the mean diagonal) keeping an empty
  band's gains at nought. The decoder slides from last frame's gains to this
  frame's across the 24 slots, so there are two ways to choose them:
  `Fit::Centred`, the default, fits a frame's worth of slots centred on the
  frame's end, where the decoder arrives at the gains — a frame of latency, no
  overshoot; `Fit::Ramp` fits through the ramp from the gains the decoder holds
  — less error inside a frame, but it overshoots a change (×1.47, ×0.78, ×1.10…
  on a steady mix) and its codes grow with it, past a skip field's 511 bytes on
  one stream.
- **The state.** It keeps what the decoder holds: the dequantised gains of the
  last frame an object was present, untouched while it is absent, as the
  decoder leaves them; and the sequence count, which after 1023 restarts at
  *one* — nought marks a splice, and a decoder that sees it drops everything.
  (The first version wrapped to nought, and every 1024th frame of the channel
  check glitched: 35.4 dB where 54.3 is the real figure.)

### Against the reference

`cargo run -p xtask --features eac3-oracle -- graft reference.ec3 --out
refit.ec3 --refit centred` decodes a reference stream's core and the objects it
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
numbered from the reference's count, its window never reaching across the cut.
The sample's first frame is such a run, a leftover of the cut it was taken
from.

| Stream | Gains on the reference's step | Payload, bytes (theirs) | Objects, slid frames: ours / theirs | Stepped frames: ours / theirs |
|---|---|---|---|---|
| film, 60 s | 92.3 % | 204 (207) | 26.6 / 35.5 dB | 21.7 / 34.5 dB |
| film, 30 s | 96.5 % | 167 (170) | 33.2 / 41.7 dB | 28.4 / 38.2 dB |
| sample, two runs | 86.3 % | 185 (181) | 27.0 / 34.7 dB | — |
| channel check | 99.8 % | 134 (134) | 54.3 / 60.7 dB | 56.8 / 58.9 dB |

The comparison favours the reference by construction: its objects *are* what
its gains rebuild, so its gains reproduce them up to the core's coding, where
any other quantisation of the same mix pays the full step — a tenth of a gain,
some −20 dB in that band for a gain near one. What the table measures is how
nearly an estimate recovers a known mix. On the channel check, whose objects
sit on speakers, it recovers it almost exactly; on film it misses the
reference's step on 4 to 14 % of gains, and the misses are not where the
reference stepped its gains but in the **narrow low bands** — one subband each
up to band 3 — where they run two to five times the rate of the wide bands:

```
off the reference's step, by band (film, 60 s):  9.3 16.5 12.5 13.0 8.8 7.8 5.1 4.9 4.4 3.0 2.7 4.8 %
```

That is the subbands' leakage. The decoder's synthesis and the analysis the fit
runs on its output are not a perfect pair, and with different gains in
neighbouring bands each subband carries some of its neighbours', which in a
band one subband wide is a large share of the band; a fit made band by band in
the QMF domain cannot see it. The next lever is to fit by synthesis — in the
domain the decoder's output lives in, all of an object's bands together —
which also gives the steep slope a place to be decided.

## Not done

- Fitting by synthesis, and the steep slope: the two known gaps above.
- A budget for the payload's size: ours run near the reference's, but nothing
  holds them under a skip field's 511 bytes.
- The downmix: these gains are fitted to a reference's core. From a master, the
  core is the objects rendered to 5.1, then coded, and the gains fitted to the
  decoded core — the step that makes `harlettizer` write objects of its own.
- The extension configuration's data.
