# ADM, as implemented

Notes from writing the `chna` and `axml` support in
[`hz-io/src/adm`](../crates/hz-io/src/adm). The Audio Definition Model is
published — ITU-R BS.2076 for the model, BS.2088/EBU Tech 3306 for the
container — so unlike the master format this is spec work rather than
archaeology. What follows is the part that is easy to get wrong anyway.

## Validated against the reference implementation

This support was first written from the specification alone, with no
third-party ADM file to check it against, and that gap is now closed. The EBU
ADM Renderer — `ear`, BSD-licensed, the reference implementation of BS.2076 —
was installed and used from both ends:

**Reading.** `ear-utils make_test_bwf` produced a file with two objects and
three direct-speaker channels. `cargo xtask adm --check` reads it and reports
*understood everything*: every element modelled, every track resolving, and the
document surviving a pass through our own model.

**Writing.** `ear-utils regenerate` reads a file this crate wrote and rebuilds
it with no warnings, and `ear-render` renders it to 5.1. Parseable and
renderable are different claims and both now hold.

Three real defects came out of that exercise, none of which round-tripping
against ourselves could have found. They are described below.

The reader nonetheless stays tolerant where the master-format reader is strict,
and the asymmetry is deliberate:

| | Master format | ADM |
|---|---|---|
| Unknown field | hard error (`deny_unknown_fields`) | counted and reported |
| Proven by | every real file to hand | one reference file, and the spec |

One file is not many. Being as strict about ADM would mean rejecting real files
on the strength of a model that has seen one example. Being *silent* about what
it skips would be worse than either — so the reader returns an `Ignored` tally,
and a conversion that loses something says what it lost.

## What the reference implementation caught

### The fractional time form was the wrong choice

Written up at length here, and wrong. The argument was that decimal seconds
cannot state 1/48000 s exactly, so sample positions need
`hh:mm:ss.<numerator>S<denominator>`. The flaw is that the precision was never
fixed: at **nanoseconds** the error is 0.5 ns against a half-sample margin of
10.4 µs at 48 kHz and 2.6 µs at 192 kHz, so a decimal timestamp recovers the
exact sample at any rate that matters. What is not exact is the five fractional
digits documents habitually carry — and the fix for that is nine digits, not a
different form.

Meanwhile the fractional form is a second-edition feature that the reference
renderer rejects outright, so the file does not open at all. Times are now
written decimal, and `interpolationLength` as a number of seconds at full
precision, which multiplies back to the same sample count. Both forms are still
read.

### A `DirectSpeakers` block needs a position, not just a label

A block carrying `speakerLabel` and nothing else is refused: *"Found
coordinates {}, but expected either {azimuth,elevation,distance}…"*. Bed
channels now carry the nominal position their label stands for, taken from the
common definitions rather than recalled — a label and a position that disagree
is something a renderer may resolve either way.

### Identifiers below `0x1000` are already taken

`AC_00010001` does not mean "our first bed channel". It means **front left** to
every implementation that loads the BS.2094 common definitions, and a file that
redefines it is asking the renderer to choose. The renderer said so:
*"non-common-definition element found with id AC_00010001 that overrides a
common-definition element"*. Minted identifiers now start above the reserved
range.

## Polar positions convert now

BS.2127-0 §10, in `hz-core::coords`. It looks like a spherical-to-rectangular
conversion and is not: the polar and Cartesian systems describe rooms of
different shapes, a sphere and a cube, and the mapping has to agree with what a
renderer would do with each. Five corners of the horizontal plane — front at 0°,
the front pair at ∓30° and the rear pair at ±110° — a tangent law between them,
and a separate elevation warp where 30° polar reaches 45° on the cube.

Ported from the EBU renderer rather than reconstructed, and checked against it:
**zero difference over 1680 positions**, to twelve decimal places. That is what
lets somebody else's polar ADM file be converted to a master set and rendered:

```
ear_ref.wav (all polar)  ->  7.1.4  ->  5.1
  I  -22.5 LUFS    true peak  -21.1 dBTP    dialnorm 23
```

with FFmpeg reading the rendered file and agreeing on both numbers.

Reproduce all of it with:

```bash
cargo xtask make-master out.atmos --objects 4 --seconds 2
cargo run -p hz-cli -- convert out.atmos out_adm.wav
ear-utils regenerate out_adm.wav regen.wav     # reads and rebuilds ours
ear-render -s 0+5+0 out_adm.wav rendered.wav   # renders ours
```

## Two ways to say when, and only one of them is exact

BS.2076 writes a time either as `hh:mm:ss.fffffffff` or as
`hh:mm:ss.<numerator>S<denominator>`. The second form exists because the first
is a resolution, not an exactness: 1/48000 s is 20833.333… ns, which no decimal
states.

At 48 kHz the decimal form gets away with it, and it is worth being precise
about why rather than repeating folklore. Real documents carry five fractional
digits, so the rounding error is at most 5 µs against a half-sample margin of
10.4 µs, and a sample position does survive a round trip. The margin narrows at
96 kHz and is gone at 192 kHz, where writing sample 1 that way and reading it
back yields sample 2.

A master set addresses events by sample position. An encoder that moved object
updates by a sample, depending on the sample rate, would be wrong in a way
nobody would ever find. So this crate reads both forms and writes the
fractional one.

**The same argument applies to `interpolationLength`**, and it is easier to
miss there: a master set's ramp is a number of samples, and 1151/48000 s is
0.023979166… The first edition of the model wrote that attribute as plain
seconds, which moves every ramp that is not a round number. This crate writes
it as a time and still reads the old spelling.

## The chain, and the two ways to walk it

A renderer gets from a track to a position by following
`audioTrackUID → audioTrackFormat → audioStreamFormat → audioChannelFormat`.
BS.2076-2 also lets an `audioTrackUID` name its channel format directly. Both
occur, and a resolver that only walks the first edition's chain silently finds
nothing on a second-edition file — silently, because a missing reference looks
exactly like a track with no objects on it.

## `chna` is fixed-width and unforgiving

Each entry is exactly forty bytes: a one-based track index, then identifiers of
exactly 12, 14 and 11 characters, then a pad byte. The identifiers are neither
length-prefixed nor terminated, so the field boundaries are all that separates
them. A writer that pads with anything but spaces produces a file whose track
references do not match its own `axml`, and the symptom is a renderer quietly
dropping a track.

Both spellings of "this track has no ADM" occur — all spaces and all zeros —
and both have to read back as nothing rather than as whitespace or as a string
of NULs that would then be written into the XML.

## Half a description is worse than none

A file with `chna` and no `axml` maps tracks to identifiers nothing defines; a
file with `axml` and no `chna` describes audio without saying which track is
which. Both are refused here by name. The alternative is letting a renderer
discover it as tracks that go silent, which points nowhere near the cause.

## `width` is two quantities with one name

BS.2076 carries an object's extent as `width`, and what the number *is* depends
on the position beside it: for a Cartesian block it is a fraction of the room,
0 to 1, and for a polar block it is an angle in degrees. A master set's `size`
is the first of those, and this crate carries one as the other — which is right
for every file it writes, since it writes Cartesian positions, and wrong for a
polar file from somewhere else.

Nothing here converts between them, and that is deliberate: turning an angular
extent into a fraction of a room is a rendering decision BS.2127 makes with
three angles and a distance, not with one number, and inventing a divisor would
be a guess that renders plausibly and wrongly. What the renderer does instead
is **clamp** the width to the panner's domain, so a polar file's 30° object is
bounded to the widest spread there is rather than handed to a panner as a 30 it
has no meaning for. The case is named in `keyframes_of` and is open.
