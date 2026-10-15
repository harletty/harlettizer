# Joint object coding

Phase B3 of the lossy track (see [PLAN.md](../PLAN.md)): the parameters that
rebuild a programme's objects from the 5.1 downmix an E-AC-3 stream carries,
in the payload ETSI TS 103 420 defines. This page is the payload layer — what
is written, and how it is held to the reference streams. Estimating the
parameters comes on top of it.

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

## Not done

- The parameters themselves: from a programme's objects, the downmix and the
  per-band matrices that rebuild them — the research half of phase B3.
- The extension configuration's data.
