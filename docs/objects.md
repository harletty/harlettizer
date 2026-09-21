# Object audio metadata

[`hz-meta`](../crates/hz-meta) reads and writes the payload that says what the
elements of an immersive programme are, where they go, and how loudly. A
delivery bitstream carries it verbatim — TrueHD in the extra data of an access
unit under Evolution identifier 11, E-AC-3 in an Evolution frame of a syncframe
— so it lives in a crate of its own and depends on neither codec.

## Where it stands

The reference stream's own payload **reads as itself and writes back byte for
byte**. That is the claim worth making: not that a decoder accepts what this
writes, but that what this writes is what a real encoder wrote, bit for bit,
from a model that was read out of it.

Held against the same access unit decoded by `truehdd` — which writes its
account of the programme out as a master file set, a format this project
already reads — the two agree: sixteen elements, the first a low frequency
channel with no position of its own, fifteen dynamic objects at the front left
corner with their height honoured, unity gain, a ramp of 1536 samples. And on a
later access unit where one object moves, they agree about which object, which
sample, and where to.

## What is modelled

The shape a real immersive programme uses: **every element a dynamic object**,
one of which may be the low frequency channel. That is what the reference
carries and what a sixteen-element presentation is. The syntax also admits
speaker-anchored beds and an intermediate spatial format, and those are refused
by name — a payload read wrong is a payload whose objects land somewhere else,
which is worse than one that does not parse.

Within that shape everything is modelled: up to eight update blocks per access
unit, each with its own offset and ramp; per element an active flag, a gain,
a position, a distance, zone constraints, elevation, size, screen anchoring and
snapping.

## Positions are codes here

The wire carries `x` and `y` in sixty-seconds of the room and `z` in fifteenths
of it, and a later block may move an element by a small *difference* in those
same units. Keeping the codes rather than the metres they stand for is what
makes reading and writing exact inverses; the rounding happens once, where a
caller crosses into the master format's frame with `Position::to_master`.

Clamping is part of the coding and not a safety net: a decoder clamps each axis
where it stands, and it is the *clamped* value the next block's difference
applies to. Storing the code and clamping the code is the same operation, which
is why it can be done on the way in.

## Two kinds of memory, carried differently

A block after the first may say "unchanged" in two bits instead of restating an
element. What "unchanged" means is where this is easy to get wrong, because the
payload carries two independent memories:

- **What the previous block of the same element left.** It resets at every
  element, and — this is the part that bites — an element that is *not active*
  in a block leaves nothing behind: a decoder resets what it holds, so anything
  after it that says "unchanged" means unchanged from *that*, not from the last
  block that stated something.
- **The previous element's gain at the same block.** This one runs the other
  way, across elements, indexed by block. It is what lets an element that
  sounds like the one before it spend two bits instead of eight, and the
  reference spells its gains that way even where it saves nothing.

Getting either wrong produces a payload that parses and puts objects somewhere
else. Both are mirrored exactly, in the reader and in the writer, which is what
makes the reference reproduce byte for byte.

## One thing the wire can spell more than one way

An element whose gain matches the element before it can be written either as
that gain outright or as "the same as the last one", and both are two bits. The
reference uses the second everywhere except for its first element, where there
is nothing before it to be the same as. This writes it the same way — not
because it saves anything there, but because reproducing the reference exactly
is the only evidence that the writer and the reader are inverses rather than
two guesses that agree with each other.

## The oracle has a version, and it matters

`harletty` — the maintained line of `truehdd`, with the same command line and
a newer `truehd` library — is the only decoder that reads a sixteen-element
presentation at all, so it is the only oracle this phase has. Which build is
installed turns out to matter:

🔴 **truehdd 0.4.0 reads the six-bit object gain index twice.** Its match arms
call the reader again inside themselves, so a payload stating a gain that way
costs it twelve bits where the format has six; it then runs off the end of the
payload and abandons the access unit. **0.6.1 reads it once.** The bug is in the
decoder and not in what this writes — the payload is the same one 0.6.1 accepts
and the same shape the reference carries — but from the outside the two are
indistinguishable, and a stale oracle that fails on correct output is worse than
no oracle at all.

`cargo xtask mlp` therefore runs whatever `HARLETTY` names (`TRUEHDD` is still
honoured), falling back to `harletty` on the path. Pointing it at a build of
your choosing is how a decoder bug is told apart from an encoder one.

And it reads every payload back a second way, with the `truehd` crate in
process, so that the check does not depend on a binary at all. That one is
behind a feature — `cargo xtask --features oracle` — because **compiling
`truehd` needs 13.4 GiB**, measured, and a hosted runner for a private
repository has 7 GB. One crate wanting twice the machine is not something a
job's parallelism can be tuned around, and it took two failed builds and a
memory profile to see that the cost was one process and not four.
