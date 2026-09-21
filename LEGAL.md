# Licensing, provenance and patent posture

This file is normative for the project. It decides where implementation
material may come from, and it must be settled before the first line of
encoder code is written — reversing a provenance mistake later means
rewriting the affected crate from scratch.

## 1. Project license — GPL-3.0-or-later (decided)

Rationale:

- **It unlocks FFmpeg.** FFmpeg's `libavcodec/mlpenc.c` (a working
  MLP/TrueHD encoder), `ac3enc.c` and `eac3enc.c` are LGPL-2.1-or-later.
  LGPL-2.1 code may be relicensed into a GPL-3.0 project. A permissive
  (MIT/Apache) license for this project would forbid that, and would put
  the two hardest pieces — lossless MLP framing and AC-3 bit allocation —
  back on the "write from scratch" pile.

Consequence to accept: no proprietary or permissively-licensed downstream
can absorb this code without going GPL too. Given the goal is an open
alternative to a proprietary tool, that is the intended outcome.

The permissive alternative was considered and rejected: it would have
forbidden any borrowing from FFmpeg and added roughly two months to
phases A1 and B1, which would have had to be written strictly from the
specifications.

## 2. Allowed sources of implementation material

**Published specifications** (the primary source in all cases):

| Format / topic | Document |
|---|---|
| AC-3, and the dynamic range gain words | ATSC A/52 (Digital Audio Compression Standard), §7.7 and §5.4.2.8 |
| The named DRC compression characteristics (`film_light`, `film_standard`, `music_light`, `music_standard`, `speech`) | *Dolby Metadata Guide*, the format owner's published guide, in its freely downloadable edition and without a click-through agreement. **Not** A/52, which does not contain them — see `docs/drc.md`. |
| E-AC-3 | ETSI TS 102 366 |
| Joint object coding on E-AC-3 | ETSI TS 103 420 |
| AC-4 | ETSI TS 103 190-1 / -2 |
| Loudness | ITU-R BS.1770-4, EBU R 128 / Tech 3341 |
| ADM | ITU-R BS.2076, EBU Tech 3285 Supplement 6 (BW64) |
| Master file layout | The publicly published master-file specification |

**GPL-compatible code**, reusable directly:

| Source | License | Use |
|---|---|---|
| FFmpeg `libavcodec` (`mlpenc.c`, `ac3enc*.c`, `eac3enc.c`) | LGPL-2.1-or-later | Port to Rust; keep per-file attribution |
| `truehd` crate (Rainbaby / truehdd) | Apache-2.0 | Bitstream structures, bit IO, CRC — and the decoder used as the test oracle |
| `oxideav-ac4` | MIT | AC-4 framing and channel-based encoding |
| `damf` crate (harletty-bridge) | Apache-2.0 | Master-file metadata model and writers |
| EBU ADM Renderer (`ear`) | BSD-3-Clause | The BS.2094 common-definition speaker positions, and the reference implementation both directions of the ADM support are checked against |

Ported code must carry the original copyright line and license notice in
the file header, and the port must be recorded in `docs/provenance.md`
(file, upstream revision, what was changed). Licences that require their
text to travel with the source — the EBU ADM Renderer's BSD 3-Clause, the
`truehd` crate's Apache-2.0 — are reproduced in `LICENSES/`.

## 3. Forbidden sources

- **Cavern.** Its license forbids selling any part, imposes
  attribution-on-use conditions, and is not a free software license — it
  is incompatible with GPL and with this project. **No code, no table, no
  constant may be copied from it**, and no file here may be a
  transliteration of a Cavern file.
- Any SDK, header, sample code, or documentation obtained under NDA or a
  click-through developer agreement from the format owner.
- Any output of a large language model that reproduces the above.

### 3.1 The reference encoder has to be a licensed one

The plan relies on differential testing against the proprietary encoder —
same input in, compare the output — in two places: the compression curves
that `hz-analysis::drc` can only otherwise guess at, and the delta between
our speech gate and theirs.

Such a measurement is worth exactly what the run behind it is worth, and that
is a separate question from the ones this file is otherwise about: those are
about where implementation material may come from, and this is about whether
the run that produced a number was one its owner had licensed. The rule:

> **A measurement is only admissible here if the reference encoder that
> produced it was running under a licence its owner issued.** No number, curve,
> table or test expectation may enter this repository from any other run, and
> no document here may cite one.

This is not caution about appearances. A project whose defensible position
rests on the provenance of everything in it cannot afford a measurement it
could not account for; one such number taints every clean thing derived from
it, and there is no way to prove afterwards which numbers came from where.

No licensed run has been available to this project. Consequently:

- The speech gate's delta against the reference is **not measured**, and
  `docs/speech.md` says why rather than reporting a number. It stays open
  until a licensed run is available.

The DRC characteristics were on this list and have come off it, because the
premise was wrong rather than because the rule softened. They do not need a run
of the reference encoder at all: the format owner publishes them itself (§2),
and reading a published guide is not a measurement. What a licensed run would
still buy is a check of the *dynamics* around the characteristic — attack,
release, hold — which no public document states; those stay ours, designed and
fitted here, and `docs/drc.md` says so.

## 4. What this software is for

harlettizer is provided for **interoperability and research**: so that audio
authored with free tools can be delivered in the formats that players,
receivers and decoders already understand, and so that those formats can be
studied and implemented independently. It is written from published
specifications and from the GPL-compatible sources listed in §2, and it is
provided for no other purpose.

- **It contains no key, certificate or licence material.** Where a format
  protects its metadata with a keyed digest, this project implements the
  standard algorithm and reads the key from a file on the user's own machine.
  It does not distribute a key, does not derive one, and does not help anyone
  obtain one. A stream written without a key is a valid stream whose
  protection field verifies against nothing.
- **It is not a way around anyone's licence.** Nothing here may be used to
  circumvent a technical protection measure, to evade the licence terms of a
  product, tool or service, or to obtain a right its user does not hold.
  Whoever uses this software remains responsible for the licences, the
  patents (§5) and the contracts that apply to their own use, in their own
  jurisdiction; the project makes no claim that any given use is lawful.
- **It neither decrypts nor removes protection from any content**, and it
  respects copyright. It harms no technical protection measure, and it does
  not help others to.

This is the posture of comparable projects. The VideoLAN libraries state that
they are research projects with an interoperability purpose, that they provide
no key, and that they therefore harm no technical protection measure; it is
adopted here in the same terms, for the same reasons. A contribution that would
change this posture is a contribution to a different project.

## 5. Patents

The formats implemented here are covered by patents held by their format
owner. Writing and distributing an independent implementation is lawful in
the jurisdictions where this project is developed; *using* it may require
a license from the patent holder depending on jurisdiction and use case.

The project will:

- carry a clear patent notice in the README and in the release notes;
- make no claim that use is patent-free;
- follow the posture of comparable projects (x264, FFmpeg, opus tooling):
  ship the source, let each user assess their own obligations.

## 6. Trademarks

No third-party trademark appears in the project name, binary name, crate
names, or branch names. Format names are used only descriptively and only
where necessary (e.g. "compatible with decoders implementing ETSI TS
103 420"), never as a badge of approval. Repository branch names follow
the existing house rule: generic and descriptive (`feat/mlp-encoder`,
`fix/bw64-chna`), never a format brand.

## 7. Decisions

- [x] **License: GPL-3.0-or-later.** The FFmpeg ports (`mlpenc.c`,
      `ac3enc*.c`, `eac3enc.c`) are therefore available to this project.
- [x] **Name: `harlettizer`**, crates prefixed `hz-`, published on the
      `harletty` GitHub account next to the decoder. No third-party
      trademark in the name; branch names stay generic and descriptive.
- [x] **First track after phases 1–2: Track A** (lossless MLP, then the
      immersive TrueHD presentation).
- [x] **The panner is in-tree, and there is no git dependency.** Panning
      objects onto a layout is vector-base amplitude panning over a
      triangulated layout, and `hz-render` carries one written from the
      published method (see `docs/provenance.md`), used by the clustering
      metric to judge the immersive presentation. It was a dependency on
      an external renderer for a while; that cost a git fetch at every
      first build and the whole of that renderer's dependency tree, for a
      few hundred lines of geometry, and it is gone.
