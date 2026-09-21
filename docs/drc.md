# Dynamic range metadata

[`hz-analysis::drc`](../crates/hz-analysis/src/drc.rs) carries the fields a
decoder acts on, and the curve that decides what goes in them.

## What ATSC A/52 gives, and what it does not

The standard was fetched and read rather than recalled, and the answer was not
the one this project's plan assumed.

**It specifies exactly:**

| Field | Where | What |
|---|---|---|
| `dynrng` | §7.7.1.2 | Per-block gain. `X0 X1 X2 . Y3 Y4 Y5 Y6 Y7` — a signed 3-bit exponent and a 5-bit fraction with an implied leading one, giving `2^(X+1) × (32+Y)/64`: +23.95 to −24.08 dB. |
| `compr` | §7.7.2.2 | Per-frame heavy compression. Same idea, split 4/4: `2^(X+1) × (16+Y)/32`, +47.89 to −48.16 dB. Twice the range at half the resolution. |
| `dialnorm` | §5.4.2.8 | Five bits, 1..31, meaning −1 to −31 dB below full scale. Zero is reserved and a decoder must read it as −31. |

All three are implemented from the standard. Every one of the 256 codes of each
gain word round-trips, and the exponent table is asserted against Table 7.29.

🔴 **It does not specify the named profiles.** `film_light`, `film_standard`,
`music_light`, `music_standard` and `speech` are the reference encoder's own
curves. A/52 describes the wire format of a gain, the mechanics of applying it,
and the *intent* — compress towards dialogue level — and stops there. It never
names a profile.

This project's plan said otherwise, in as many words: "implemented from A/52
§7.7, which specifies them exactly. This is spec work, not reverse
engineering." That was wrong, and reading the standard is what showed it.

## They are published, though — just not there

An earlier version of this document went one step further than the finding
supported, and said the named characteristics were a shape **nobody** had
published. That is a different claim from "not in A/52", and it is false.

The format owner publishes them itself, in its *Dolby Metadata Guide*, in the
freely downloadable edition and behind no click-through agreement. One page
gives, for each of the five: maximum boost, the boost range and its ratio, the
width of the null band, the early cut, and the cut and its ratio. That makes
them a published specification in the sense of [LEGAL.md](../LEGAL.md) §2, on
the same footing as A/52 itself — and it means implementing them needs no
measurement of the reference encoder and no licensed run of it.

Two things about that document are worth writing down before anyone leans on
it. It prints the levels for a dialogue level of −31 dB, but says in the same
breath that the null band is centred on whatever the dialogue level parameter
is set to; so the levels belong in the code **relative to dialogue**, with
`dialnorm` displacing them, not as absolute dBFS. And its `film_light` entry
does not agree with itself: it gives a null band ending at −21 dB and then an
early cut starting at −26. Two constraints settle it — an early cut begins
where the null band ends, and the characteristic bottoms out at the floor of
the `dynrng` word A/52 gives (−24.08 dB) — and they admit exactly one reading.
Where that guide and any other account of these curves disagree, this project
follows the guide, and writes down the derivation: that is what makes the
provenance checkable rather than merely asserted.

What the guide does **not** contain is the dynamics around the
characteristic — how fast the gain moves towards it and away from it, what
weighting the level is measured through, what limits a boost. No public
document states those. They are ours to design and to fit, and being an
offline encoder is an advantage there rather than a handicap: the machinery a
real-time encoder needs in order not to be caught out by a transient it cannot
see coming is machinery a look-ahead does not need.

## So what is implemented instead

The shape a compression characteristic has, with parameters: a null band around
dialogue level where nothing happens, a boost region below it and a cut region
above, each with a ratio and a limit. Two defaults are provided, `wide` and
`narrow`, and they are **ours** — they are not any named profile under another
name.

The named ones are not implemented yet, and the reason is no longer provenance.
[`Characteristic`](../crates/hz-analysis/src/drc.rs) carries one ratio per side
where a named characteristic needs two above dialogue — an early cut and then a
much steeper cut — so it wants the same node table [`Measured`] already has,
expressed relative to dialogue instead of in dBFS. And a published
characteristic driven by the detector this encoder has today would sound worse
than the measured curve below, not better: the level it answers to is a flat
sum with no weighting and nothing excluded, and the gain follows it with no
attack, no release and no ceiling. The characteristic is the last piece to fit,
not the first.

Two properties are tested because getting them wrong is audible:

- **Dialogue is left alone.** A curve that moves the null band defeats the
  reference the entire system is built on.
- **The curve is monotonic in output level.** A louder input that comes out
  quieter than a softer one is a pumping artefact with a schedule.

## Matching the reference encoder: measured from shipped streams, not from it

This is a different question from the one above, and it stays worth asking
after it. The guide says what the five named characteristics *are*; it does not
say what a stream in the field actually states, which is what a decoder acts on
and what this encoder has to sit alongside. A stream carries no record of which
characteristic it was authored with, nor of the dynamics that shaped the gain
on its way to the word. Only the words themselves say that.

They can be measured two ways, and only one of them is open.

**Feeding a level ramp through the encoder and reading the gains back** is the
obvious one, and LEGAL.md §3.1 closes it: it needs a licensed run of the
encoder, and none has been available to this project.

**Reading the gains out of shipped streams** is the other, and nothing closes
it. A shipped stream is one this project already reads end to end; the gain
words are in it, and recovering them runs no encoder, traces nothing and looks
inside nothing. `cargo xtask drc` does it:

```bash
cargo xtask drc programme.thd                                  # what the words say
truehdd decode --presentation 0 --format pcm --output-path programme_p0 programme.thd
cargo xtask drc programme.thd --substream 0 --audio programme_p0.pcm --channels 2
```

The decoder is only a decoder: it validates the gain words and never applies
them, so the level measured beside them is the uncompressed programme's.

### The word carries a curve, and here it is

Eight streams, both of the narrow presentations, about 16 000 gain words. The
level is a one-pole leaky integrator over each access unit's power; the
correlation peaks sharply at a time constant of **600 to 900 ms** on every
stream tried, between 0.80 and 0.98.

| level of the presentation | 2-channel presentation | 6-channel presentation |
|---|---|---|
| −42..−39 dBFS | −1.2 dB | +1.1 dB |
| −36..−33 | −2.1 dB | +0.1 dB |
| −33..−30 | −3.4 dB | 0.0 dB |
| −30..−27 | −4.4 dB | −0.6 dB |
| −27..−24 | −6.2 dB | −1.1 dB |
| −24..−21 | −9.0 dB | −1.7 dB |
| −21..−18 | −11.9 dB | −2.8 dB |
| −18..−15 | −14.2 dB | −4.1 dB |
| −15..−12 | −16.7 dB | −6.0 dB |
| −12..−9 | −18.6 dB | −8.4 dB |
| −9..−6 | −20.8 dB | −9.9 dB |

The six-channel presentation is a textbook characteristic: **a null band up to
about −33 dBFS**, then a ratio that rises from about 1.2:1 through 1.5:1 to
around 3:1 above −15 dBFS. The two-channel one is the same shape pulled down
and steepened — it never sits at unity, and it reaches −0.94 dB/dB between −24
and −18, which is a ratio steep enough to be a limiter. That difference is what
a downmix costs: the fewer the channels, the more the sum needs holding back.

### It is an absolute curve, not a dialogue-referenced one

Tested, because it is the assumption A/52's model invites. Re-expressing each
stream's level against its own median and pooling makes the gain at a given
level **more** scattered, not less: 1.10 dB of spread becomes 2.17 on the
six-channel presentation, 1.74 becomes 3.62 on the two-channel one. So the gain
answers to the absolute level of the presentation, and a curve fitted per
programme would be fitting noise.

### What this is not

- Not the named profiles. Which characteristic each stream was authored with
  is not in the stream, so these are what shipped streams *state*, pooled — not
  `film_standard` under another name.
- Not a detector. The one-pole integrator is the simplest thing that could be
  right, and 1 to 1.7 dB of residual spread at a given level says the
  reference's is not exactly it — an asymmetric attack and release would be the
  next thing to try.
- Not yet in the encoder. `harlettizer encode` still states unity; see below.

## What the encoder states, and what it does not

`harlettizer encode` writes a dynamic range word on every substream a decoder
may stop after, and restates it every 128 access units — the reference streams'
own cadence, and the longest the three-bit deadline field allows. Before this it
wrote none at all, so a decoder asked for compressed output found nothing to
apply: the machinery had been built, tested and exercised through
`cargo xtask mlp --drc`, and never connected to the command that makes the
streams.

| | words | of units | cadence |
|---|---|---|---|
| a reference stream | 282 | 36 106 | every 128 units |
| `harlettizer encode` on `crowd40` | 38 | 4 800 | every 128 units |

It costs 304 bytes on a 2.85 MB stream, because a word is written when it is due
rather than on every unit.

**The gain it states is unity**, and that is the honest answer rather than a
placeholder: this document's own finding is that A/52 specifies the wire format
and the intent and stops, and that the named characteristics are the reference
encoder's curves. Unity says "no reduction asked for" — a stated profile rather
than an absent one. `--drc <dB>` states a constant instead, and `--drc off`
states none, which is what every stream written before this said.

### The encoder states the measured curve

`--drc measured`, which is the default. Each presentation's level goes through
a one-pole detector at [`TAU_MS`](../crates/hz-analysis/src/drc.rs) and the
[`WIDE`] table, and the word is restated every 128 units. On a real master,
against the reference stream of the same content:

| | words | substream 0 | substream 3 | changes |
|---|---|---|---|---|
| reference | 282 | −1.69..+0.00 dB | +0.00..+0.28 dB | 62 / 32 |
| ours | 283 | +1.13 dB, fixed | −1.03..+1.13 dB | 0 / 68 |

The cadence matches and the shape is a curve rather than a constant. Two things
do not match, and both have the same cause.

🔴 **This encoder's narrow presentations are not downmixes.** A decoder stopping
after substream 0 gets the first two channels as they were written, not a fold
of all sixteen; the reference computes its folds from dense matrices this
encoder does not yet write. So the level the curve reads is not the level a
decoder hears, and it reads *low* — which is why our substream 0 sits at a
boost where the reference's cuts, and why it does not move at all.

That is also why [`STEREO`] — the steeper curve shipped streams state for a
real two-channel downmix — is **not** applied. Stating it would compress for a
summation that never happened. When the presentation matrices land, the level
becomes the right one and the narrow presentations should take the curve that
was measured for them.

Until then the honest description is: the stream states a gain that follows its
own presentations' levels through the curve shipped streams follow. `--drc off`
states none, and `--drc <dB>` states a constant.

## One number that is not a choice

`dialnorm` is the integrated loudness rounded into five bits. Measure a
rendered presentation with `cargo xtask loudness` and read the field off the
figure:

```
  I                -9.2 LUFS        ->  dialnorm 9, stating -9 dB
```

Both ends clamp, and the clamping is the format's: the field cannot say
dialogue is at full scale, and cannot say it is quieter than −31 dB.

## A resolution figure that is a worst case

A/52 quotes 0.25 dB resolution for `dynrng`, and the tests here allow 0.14 dB
of error rather than 0.125. The mantissa is a fraction with an implied leading
one, so its steps are widest at the bottom of its range — 32→33 is 0.267 dB
while 62→63 is 0.14. The quoted figure is the worst case, not a constant.
