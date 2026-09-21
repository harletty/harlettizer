# Plan

Target: an open encoding engine for immersive delivery — object master in,
delivery bitstream + metadata out — driven by a job description, usable
headless from a pipeline.

Read [LEGAL.md](LEGAL.md) first: it decides what implementation material
is usable. The license question is settled — **GPL-3.0-or-later**, so the
FFmpeg encoders may be ported — and the plan below assumes it.

---

## 0. Definition of done

Three checkpoints, in increasing order of ambition:

1. **Useful** — the master-file ↔ ADM conversion and the measurement jobs
   (loudness, DRC, presentations) stand on their own. No encoder needed.
   *The conversion half is done.*
2. **Usable from a pipeline** — a host hands the binary a master set and a
   job description, reads its progress, and gets a playable immersive
   TrueHD track back, with nothing on its side rewritten for the purpose.
3. **Complete** — E-AC-3 with joint object coding and AC-4 as well, so the
   full delivery matrix is covered.

Checkpoint 2 is the one that matters most here, because it is what makes
the engine a tool rather than a library.

---

## 1. What an encoding engine has to do

The surface an immersive delivery pipeline needs from it:

- **Inputs**: master file set (`.atmos` + `.atmos.audio` + `.atmos.metadata`),
  ADM BW64, plain WAV/CAF, with frame rate / offset / first-frame-of-action
  handling.
- **Jobs**: immersive TrueHD; E-AC-3, with or without objects; AC-3; AC-4;
  loudness measurement (BS.1770-4, speech-gated, with a threshold); a
  dynamic range characteristic per presentation; clustering to 12 / 14 / 16
  elements; presentation generation for 8ch / 6ch / 2ch with the optional
  surround attenuation and a stereo or Lt/Rt choice; silence prepended or
  appended; embedded timecode.
- **Outputs**: MLP, EC3/AC3, AC-4, WAV, plus container wrapping.
- **Operationally**: a job description in, progress lines and a log out,
  for a host that drives a bar and reads them.

That is the surface. It is large but it decomposes cleanly:
most of it is I/O and metadata, and only three pieces are real DSP
research (clustering, joint object coding parameters, speech gating).

---

## 2. Architecture

A Cargo workspace, one crate per concern, hot paths allocation-free and
free of per-sample branching (house rule: this may end up on constrained
hardware).

```
crates/
  hz-core       sample buffers, channel & object model, time base, errors
  hz-io         WAV / RF64 / BW64, CAF, master-file set, ADM XML (axml, chna)
  hz-meta       object metadata model, ADM <-> OAMD projection, trims,
                presentation descriptors, EMDF packing
  hz-analysis   BS.1770-4 loudness (M/S/I), true peak, speech gate,
                dialnorm, DRC profile curves from A/52 §7.7
  hz-render     speaker layouts, the measured fold model, the object panner
  hz-cluster    object clustering to 12 / 14 / 16 (the research crate)
  hz-ac3        AC-3 and E-AC-3 encoders
  hz-joc        joint object coding analysis + payload (TS 103 420)
  hz-mlp        MLP / TrueHD lossless encoder + immersive extension
  hz-ac4        AC-4, built on oxideav-ac4
  hz-job        job model: TOML, progress reporting
  hz-cli        the binary
xtask/          development harness, differential runs, report generation
```

Dependency rule: `hz-ac3`, `hz-mlp`, `hz-ac4` never depend on each other;
they all depend on `hz-core`, `hz-analysis`, `hz-render`. `hz-cli` is the
only crate allowed to know about all of them.

---

## 3. Two independent tracks

After the shared phases 1–2, the lossless and lossy work share almost
nothing — MLP is prediction + entropy coding, AC-3/E-AC-3 is MDCT +
bit allocation. They can be developed in either order or in parallel.

```
  Phase 1  I/O + conversion  ─┬─► Track A: Phase A1 MLP lossless ─► A2 immersive TrueHD
  Phase 2  analysis + presentations ─┴─► Track B: Phase B1 AC-3 ─► B2 E-AC-3 ─► B3 joint object coding
                                                                     └─► Phase 6 AC-4
  Phase 7  job layer  (needs whichever track is finished)
```

**Chosen order: 1 → 2 → Track A → Phase 7 → Track B → 6.**
Track A reaches checkpoint 2 and it has the
strongest objective success criterion in the whole project: a lossless
codec either round-trips bit-exactly through an existing decoder or it
does not. Track B has the wider audience (streaming delivery is E-AC-3
with joint object coding) but the fuzziest pass/fail.

---

## 4. Phases

### Phase 0 — Repository, license, CI, development harness — **done**

- LICENSE, LEGAL.md decisions closed, CONTRIBUTING, `docs/provenance.md`.
- `hz-core`: the shared error type, dependency-free. Deliberately thin —
  it grows in phase 1, driven by a real consumer.
- CI: `fmt --check` (gated from the first commit, no legacy tree to
  reformat), build and test with warnings denied, doctests. Clippy
  advisory until the first codec crate says which lints are wrong for a
  hot audio loop.
- `cargo xtask corpus` — surveys a directory of master sets, classifies
  each by what it can actually prove, and reports the object/bed
  distribution, the creation tools, and reference-resolution failures.
  Every later phase reports through it. Its tests build their own
  fixtures, so CI never needs external material.
- **Accepted**: CI definition green locally (fmt, build, 8 tests incl.
  doctests), harness surveys a directory of masters and classifies all of
  them.
- **What the survey changed**: three findings feed back into this plan —
  the masters surveyed carry no audio, they are clustering *output* rather
  than input, and a third of the configs reference their own components by
  a stale name.

### Phase 1 — I/O and format conversion — **in progress**

The engine's conversion job types, and the foundation for everything else.

- ✅ **Master file set, config and event stream** — `hz-io`, with its own
  strict model of the format (see [docs/master-format.md](docs/master-format.md)
  for why it is not `damf`'s). Every master to hand reads under
  `deny_unknown_fields` and writes back **byte-identically**, event streams
  included. `cargo xtask roundtrip`.
- ✅ **Audio component** — the CAF container, reader and writer. A real
  584 MB component rewrites byte-identically in 3.4 s. Chunks the reader
  does not interpret are carried through rather than dropped.
- ✅ **WAV / RF64 / BW64** — reader and writer, 16/24/32-bit integer and
  32-bit float, plain and extensible format chunks, `ds64` 64-bit sizes,
  word alignment, and chunk pass-through so `chna` and `axml` survive a
  rewrite. Verified against files written by ffmpeg: RF64 with `ds64`,
  extensible with a channel mask, and float with a `fact` chunk all
  rewrite byte-identically (`cargo xtask container`).
- ✅ **ADM** — the `chna` table, the `axml` payload (BS.2076 elements for
  object-based and direct-speaker audio), and the BW64 plumbing that gets
  them in and out. Validated in both directions against the EBU ADM
  Renderer: it reads and rebuilds what we write, and renders it; we read
  what it writes and understand every element. Three real defects came out
  of that and are written up in [docs/adm.md](docs/adm.md).
- ✅ **The projection**, master set ↔ ADM, and `harlettizer convert` over
  it. The coordinate mapping is derived from the decoder's own conversion
  rather than assumed. Round-trip on a synthetic set: positions, gains,
  sizes, importances and ramp lengths all recovered, and the audio
  byte-identical. What the model cannot hold — language, frame rate,
  trims, warp mode — is reported, never dropped in silence.
- ✅ **Polar ADM positions onto the master's cube** — BS.2127-0 §10, in
  `hz-core::coords`. Not the spherical conversion it looks like: the two
  coordinate systems describe rooms of different shapes. Checked against
  the EBU renderer's own implementation — **zero difference over 1680
  positions** — and it is what lets a third-party polar ADM file be
  converted and rendered at all.
- `harlettizer convert` between the two master representations, plus PCM extraction.
- **Accept when**: on real material, master → ADM → master reproduces PCM
  bit-exactly, and the metadata events compare equal through
  `damf::Event::compare_event_vectors`. Where the reference encoder can
  produce the same conversion, our output is compared against it.
- **Size**: 1–2 weeks. **Risk**: low. **Value: immediate** — this is
  checkpoint 1 on its own.

### Phase 2 — Analysis and presentations — **done, except what needs the reference engine**

- ✅ **BS.1770-4 loudness and true peak** — `hz-analysis`: K-weighting
  designed rather than tabulated so it serves every sample rate,
  momentary / short-term / integrated with both gates, EBU Tech 3342
  loudness range, and 4× oversampled true peak. Checked against closed
  forms *and* against FFmpeg's `ebur128` on tones, pink noise, a weighted
  5.1 layout and 20 LU of swing — integrated loudness agrees to the
  printed resolution every time. See [docs/loudness.md](docs/loudness.md).
- ✅ **Speech-gated loudness** — `hz-analysis::speech`. An open substitute for
  the proprietary gate: level against an adaptive noise floor, speech-band
  energy, cepstral peak prominence, syllabic modulation and spectral flux,
  smoothed in time and reduced onto BS.1770's own 100 ms grid. Dependency-free,
  about 200× real time.

  The thresholds are ours and were set from sweeps over labelled material, not
  chosen: 93 % of speech, 0 % of noise, tones, chords, applause and a moving
  string part, 4.9 % of sung vowels. What matters is not the detection rate but
  the level it recovers — speech at −27 LUFS under material at −12 comes back
  within **1 LU**, where an ungated meter is wrong by 15. See
  [docs/speech.md](docs/speech.md), including the failure mode it does not
  pretend to have solved.
- ✅ **`dialnorm` derivation** — `hz-analysis` derives `dialnorm` from the
  dialogue loudness, saying which figure it used. When the gate finds no
  speech it says so instead of quietly falling back, because a `dialnorm`
  derived from something that is not dialogue is worse than an absent one.
  `harlettizer render --measure` reported it until that command went; the
  meter and the derivation stay, for the encoder to pick up.
- 🔴 **The delta against the reference engine is not measured.** It needs a
  licensed run of the engine, and none has been available to this project; a
  project that stakes itself on provenance takes its numbers from nothing
  else. The rule is written down in [LEGAL.md §3.1](LEGAL.md); this stays
  open until a licensed run exists.
- ✅ **The dynamic range metadata A/52 actually specifies** — the
  `dynrng` and `compr` gain words and the `dialnorm` field, exact, with
  every one of their 256 codes round-tripping.
- 🔴 **Correction to this plan.** It said the named profiles —
  `film_light`, `film_standard`, `music_light`, `music_standard`,
  `speech` — were "implemented from A/52 §7.7, which specifies them
  exactly". That is wrong, and it was checked by reading the standard: A/52 specifies
  the *encoding* of a gain word and the *intent* of compressing towards
  dialogue, and never names a profile.

  The correction went one step too far, though, and said those curves were
  not published at all. They are: the format owner gives them in its own
  metadata guide, freely downloadable, and LEGAL.md §2 now lists it. So
  implementing them is transcription from a specification, not a
  measurement of an encoder, and it needs no licensed run.

  What is implemented today is the shape a characteristic has — null
  band, boost region, cut region, each with a ratio and a limit — with
  parameters, and defaults that are explicitly ours, plus a curve
  **measured off shipped streams**, which answers a different question:
  not what the five named characteristics are, but what streams in the
  field actually state. What no public document gives is the dynamics
  around the characteristic — attack, release, the weighting the level is
  measured through, what limits a boost. Those are ours to design and to
  fit, and an offline encoder can do better there than a real-time one:
  a look-ahead does not need the machinery that exists to cope with a
  transient nobody saw coming. See `docs/drc.md`.
- ✅ **Presentation folding, the channel-based half** — removed, with the
  `render` command. `hz-render` had layouts and the A/52 matrices between
  them, Lo/Ro and Lt/Rt, checked against FFmpeg's `pan` to 1 LSB; the
  encoder never called them. What it does call is what remains: the
  layouts, and the measured fold model below.
- ✅ **Panning objects onto a layout** — `hz-render`'s panner is in-tree: a
  small VBAP written from the published method, over a triangulated layout.
  The external renderer dependency is gone. The clustering metric judges the
  immersive presentation on it; the conventions are still where the work
  is — positions share a frame, azimuth does not, and a flip in the wrong
  direction mirrors the whole programme without failing any test about
  energy.
- ✅ **The fold a reference stream actually uses** — `hz-render`'s
  `ObjectFold`. Not VBAP: a shipped stream carries the object elements and
  the encoder's own 7.1 fold of them in the same access units, so solving
  for the matrix between them recovers the law rather than a plausible
  substitute. It is separable, equal power, and linear in the room's own
  coordinates, with height scaling and never steering. **88.8 % of 186 922
  measured objects land within 0.05 on every channel**, 64.3 % within 0.02.
  The **direction** is the part that looks like the format: three unrelated
  streams agree on it to a cosine of 0.99992. The height scale does not — a
  third stream uses a flat constant where two use a falling curve — and
  neither does the fold matrix.
  600 of them are checked in and scored on every run. See
  [docs/fold.md](docs/fold.md). 5.1 and 2.0 are that render taken through a
  fixed matrix rather than a render on their own geometry, and the matrix is
  not the published one: the reference spreads a rear surround across both
  sides at `(sqrt(3)/2, 1/2)` where A/52 sends it to one side at half power.
  There are **two** such matrices, both attested in shipped streams, and the
  major sync does not choose between them: thirteen streams measured have
  identical presentation fields, and the only one that varies predicts nothing
  — the ten that leave it clear give seven of one fold, two of the other and
  one behaviour nothing here implements. A narrow presentation is the earlier substreams' matrices, so an encoder
  picks and nothing has to declare it; `FoldMode` is that pick, made explicit. Which stream this is measured on decides whether it can
  be measured at all — in `substream_info` `0xcc`/`1` streams the narrow
  presentations are separately authored and no fold reproduces them. Scores
  0.0507/88.2 % and 0.0500/94.8 % on one stream, 0.0082/99.9 % and
  0.0190/99.6 % on the other; the 7.1 render behind them, fitted on the first,
  transfers to the second at **0.0133 and 98.7 % within 0.02**.
- ✅ **`harlettizer encode`** — a master set or a BW64 file encoded as a
  TrueHD stream, with the master's own object metadata carried in the access
  units. The join that the object work was missing: reader, projection,
  metadata writer and bitstream writer all existed and nothing wired them
  together. A programme slice re-encoded and decoded again comes back **bit
  for bit** across all its elements, with the objects where the master put
  them. A programme slice encoded and decoded again is **1.0035×** the size
  of its reference stream — 0.02 bit a sample — down from 1.88× at the start
  of the work:
  the twenty-bit content this used to code as twenty-four, the filters it
  restated in every block, the restart interval, the parameters it restated
  for channels whose coding had not changed, and last the second filter as a
  fixed design.
  It codes as many elements as the master has — nine to sixteen, where shipped
  streams use twelve, fourteen and sixteen and the description is identical
  between them. See [docs/encode.md](docs/encode.md).
- ✅ **`harlettizer render`** — was a master set or a BW64 file rendered onto
  a layout, optionally folded, optionally measured, the whole chain agreeing
  with FFmpeg reading the rendered file. **Removed**, with the external
  renderer dependency: the encoder never called it, and a renderer is not what this
  project is for. `cargo xtask loudness` still measures a rendered file.
- **Accept when**: loudness within ±0.1 LU of a reference implementation
  on the EBU test set; the gain words round-trip every code;
  rendered presentations match the reference engine's presentation WAV
  output within a stated tolerance, and the speech-gate delta is
  characterised on real material rather than assumed.
- **Where that leaves it**: the first two are done, and the third is done for
  7.1 — the tolerance against the reference engine's own presentation is stated
  and tested, because a shipped stream turned out to carry that comparison
  inside itself. Decoding one is not running an encoder, so LEGAL.md §3.1 does
  not bar it. What still needs a licensed copy is the narrow presentations and
  the speech gate; the gate's behaviour is characterised against ground truth
  we built instead, which is a weaker oracle than the reference but a real one.
- **Size**: ~1 week + VAD tuning. **Risk**: low, except the speech gate,
  which is a known, bounded, measurable difference.

### Track A — lossless

#### Phase A1 — MLP / TrueHD lossless encoder — **done for channel-based streams**

- ✅ **A decodable stream, and a decisive test.** `hz-mlp` writes TrueHD:
  the bit writer, the four integrity fields, the major sync, the substream
  and restart headers, the decoding parameters and the block data.
  `decode(encode(x)) == x` **bit for bit** for mono and stereo, 16- and
  24-bit, 44.1 / 48 / 88.2 / 96 / 176.4 / 192 kHz, and lengths that are not a
  whole number of access units — through FFmpeg's decoder, with `truehdd` as
  a third opinion on the structure. `cargo xtask mlp file.wav`.
- ✅ **Prediction, and a restart interval.** FIR prediction with the order and
  coefficients chosen per block, run in exactly the decoder's integer
  arithmetic; residuals written at the width the block needs rather than at
  twenty-four bits; and a restart header every sixteenth access unit instead of
  every one. On a programme's centre channel that took the stream from **2.04×
  the size of FFmpeg's to 1.12×**, and on incompressible noise to 1.04×. Still
  bit exact at every step. See [docs/mlp.md](docs/mlp.md), including the two
  measurements that shaped it — the fit needs 256 samples of context, and
  framing was the larger cost before the interval landed.
- 🔴 **Correction to this plan.** It said to restructure the port "around the
  `truehd` crate's existing bitstream structures so encoder and decoder share
  one model of the format". That is not what was done and should not be: the
  crate is vendored upstream and may not be modified, its structures are
  parse-only and carry decoder state, and coupling an encoder to them would
  buy a shared vocabulary at the cost of breaking on every upstream release.
  What the plan actually wanted from "sharing a model" was *validation*, and
  that is better had by using the decoder as an **oracle** — which is what the
  harness does, alongside FFmpeg, so a shared bug in one cannot hide.
- ✅ **The entropy codebooks.** All three, chosen per block along with the
  width of the raw tail beneath them. Worth about half a bit a sample — the
  right order for tables of fifteen to eighteen symbols that only price the top
  of each residual — and it makes a silent channel free, which a programme has
  a lot of. Programme row: **1.12× → 1.09× FFmpeg**. The format's residual
  *offset* field was implemented too and then removed: it made the stream
  larger, which is what zero-mean residuals look like from the other side.
- ✅ **Rematrixing**, and an honest answer about what it is worth. One
  primitive matrix, both directions tried with a least-squares weight, costed
  exactly against coding the channels apart, decided once per restart interval
  and held. It beats FFmpeg on correlated stereo noise (**0.99×**) and is worth
  43 % where the channels are nearly the same — and **nothing at all on a real
  programme's stereo downmix**, where the search declines it every time and is
  right to: forcing it on makes that stream larger. See
  [docs/mlp.md](docs/mlp.md), including the warning about the dual-mono
  fixtures that made it look like a 50 % win before the material was checked.
- ✅ **The second prediction filter pays after all — as a fixed design, not
  a fit.** Fitted to what the first filter leaves it never paid (1 % of
  blocks, +232 bytes; charged honestly, 0 %), because what is left after
  eight taps is white. Read from a shipped stream with `harletty info
  --filters` (2026-09-07) it is nearly constant on every channel and block —
  `[1.72, 0.77]` at order two, `0.83` at one — two poles near Nyquist that
  model programme audio's steep low-pass cheaply. Three fixed designs, the
  first filter fitted to `x / (1 + B)`, a state block to start the pair (the
  format allows one on the second filter only, and the reference sends one at
  every restart), candidates charged for what they will write, and the design
  decided per channel at each restart by coding the channel's recent past
  again in every mode — no width model will do as the judge, the one that
  promised the gain was a windowing artefact: −0.7 to −1.4 % on every
  programme fixture, the programme slice 1.013× → **1.0035×** the reference.
  Costs 3× the encoding time, or 1.5× with `--fast`, which gives up a quarter
  of a per cent. Two things from the first attempt still stand: the filter
  shift must be at least eight, and "unchanged" must mean unchanged.
- ✅ **The second substream**, and with it 5.1 and 7.1. Both round-trip
  bit-exactly; 5.1 is **1.12× FFmpeg**. Three things had to be right and two of
  them are invisible in a stream that parses cleanly: a full decode applies
  only the *last* substream's matrices, so one on the stereo pair must be
  declared in both; past six matrix channels the noise-type bit must be set,
  which is the `TODO 0x31eb` that stops FFmpeg's encoder at six; and the
  format's channel order puts the side surrounds before the rears where a WAV
  file does the opposite. See [docs/mlp.md](docs/mlp.md).
- **Phase A1 is done** for channel-based TrueHD. What is left of its acceptance
  criterion is the comparison against the reference engine, which needs a
  licensed copy of it (LEGAL.md §3.1); FFmpeg's encoder stands in, and this is
  within 8–12 % of it on real material and ahead of it on correlated noise.
- Next: **phase A2** — the sixteen-channel presentation, object metadata in the
  extra-data payload, and the clustering that is the hard part of the whole
  project.
- **Accept when**: `decode(encode(x)) == x`, bit-exact, for every channel
  count and sample format, on real material — verified with the `truehd`
  decoder *and* with FFmpeg, so a shared bug in one cannot hide. Bitrate
  within a stated percentage of the reference engine's for the same input.
- 🔴 **That criterion was not being run, and it mattered.** The harness put
  every stream through FFmpeg's decoder and asked `truehdd` only whether the
  framing parsed. Running both round trips found, on the first attempt, that
  the encoder was carrying its filter state across restart headers: correct in
  FFmpeg, which keeps its state buffers, and wrong in `truehdd`, which clears
  them — as the format requires, since a restart is where a decoder joins the
  stream. Fixed by writing a restarting unit as two blocks, the first
  unpredicted; it costs 0.1 % of the stream. Both decoders now run on every
  stream, at every channel count, over **every presentation the stream
  declares**. See [docs/mlp.md](docs/mlp.md).
- **Where that leaves it**: bit-exactness is met for every shape the encoder
  writes, through both decoders; against FFmpeg's encoder every measured row
  is now under 1.00× (0.94× to 0.98×), and against its reference stream a
  re-encoded programme slice is 1.0035×, measured rather than guessed.
  The reference engine's bitrate cannot be part of the comparison until
  a licensed copy exists (LEGAL.md §3.1), so FFmpeg's encoder stands in — and
  it stands in only up to six channels, since it downmixes anything wider to
  5.1 rather than encoding it.
- **Size**: 2–4 weeks. **Risk**: medium, but failure is always visible —
  bit-exactness does not admit "sounds fine".

#### Phase A2 — Immersive TrueHD — **started**

- ✅ **A reference stream, read and pinned.** `cargo xtask thd` reads an access
  unit's structure and checks every checksum in it. Phase A1 went right because every
  field was held next to a stream somebody else wrote; this is what makes that
  possible for A2. Every checksum in the reference verifies against this
  project's own implementations — including the major sync over a 32-byte
  header it had never seen. The whole immersive layout, and the gap to what
  this writes today, is tabulated in [docs/mlp.md](docs/mlp.md).
- 🔴 **What reading it changed.** Three things were not guessable and are now
  known: the restart sync word is not one value (`0x31ec` marks the
  object-based substream, and FFmpeg's decoder cannot read it at all, so it
  cannot verify a sixteen-element presentation); the channel assignment is a
  permutation rather than the identity; and the major sync's length is read
  from inside itself, so an Atmos one is longer and a reader that assumes 28
  bytes produces plausible nonsense.
- ✅ **Four substreams and the sixteen-element presentation.** A
  sixteen-channel stream is written — substreams `0..1`, `2..5`, `6..7`,
  `8..15`, the three restart sync words, the extra channel meaning declaring
  fifteen dynamic objects and a low frequency channel — and **all four of its
  presentations decode bit-exactly**: two channels, six, eight, sixteen. That
  last is the point of the split, and checking only the widest would have
  proved none of it.
- 🔴 **The oracle is weaker here than in phase A1.** FFmpeg's TrueHD decoder
  caps at eight channels and its encoder at six, so above eight there is one
  decoder and no second encoder to measure against. The reference stream partly
  makes up for it — it pins the fields, and every checksum in it verifies
  against this project's — but two independent implementations is not available
  for the sixteen-element presentation and the acceptance criterion says so.
- ✅ **Sync word C's matrix syntax**, and with it rematrixing in an immersive
  stream — which is all or nothing, because a decoder undoes only the matrices
  of the substream it stops at and every presentation must be able to undo
  everything beneath it. Worth **−39 %** on sixteen channels whose elements are
  correlated, which is what a bed and objects derived from it look like;
  declined at no cost where they are not.
- ✅ **The arrival clock is a schedule, and a unit does not have to fit on its
  own.** A unit's bytes are delivered over the gap to the next unit's arrival,
  so a large one is given a longer gap and the units after it shorter ones. The
  rule was read out of the reference rather than guessed: the gap is at least
  `ceil(words × 256 / the declared peak)`, never once short over its first 25876
  units, gaps from 17 to 129 against a frame of 40 — and their mean is 40.002,
  because the clocks have to keep the same average rate. A stream gives itself a
  reserve to borrow against; this gives itself eight access units against the
  reference's 60 samples. The sixteen-channel stream that could not be written
  before now round-trips through all four presentations with no complaint.
- 🔴 **What a schedule cannot fix** is a stream needing more than it declared on
  *average*. That is a bitrate and not a peak, and it is now reported as units
  that could not be placed.
- ✅ **The substream directory's dynamic-range word**, which is what makes an
  entry four bytes rather than two: a per-presentation gain, the deadline that
  says how long it stands, and the major sync's start-up gain that has to
  follow it. Metadata, not coding — a decoder asked for full range ignores it —
  so the round trip cannot tell you it is wrong, and both decoders were needed
  to find that testing the deadline before counting puts every word one unit
  late. **What gain to ask for stays phase 2's**, and phase 2's DRC item is
  still blocked on a licensed reference encoder; the encoder writes what it is
  told.
- ✅ **The extra data block and the Evolution frame in it.** After the last
  substream an access unit may carry a payload, and what a decoder expects
  there is decided by the major sync's flags rather than by the block — bit 12
  set is an Evolution frame, which is what the reference's `0x1000` was. The
  container is written and read: an arbitrary payload under any Evolution
  identifier goes in and comes back with every check verifying, at sizes on
  both sides of the boundary where the length needs a second group of bits. The
  sizing rule reproduces the reference's 38 words and 73 bytes from the frame's
  bit count alone, and a block this encoder writes is pinned as a fixture. **The frame's
  protection field is a keyed digest** — HMAC-SHA-256 over the access unit
  and the frame, truncated to the field — written when the local settings
  file holds the key and checked by `cargo xtask thd`: 18 764 of 18 764
  fields on the three reference streams. The key is not in the repository and
  is not this project's to ship; without it a constant stands in the field.
- ✅ **The object audio metadata payload**, in a crate of its own
  ([`hz-meta`](crates/hz-meta), since E-AC-3 carries the same payload later):
  the programme, up to eight update blocks an access unit, and per element a
  gain, a position and the rendering flags that go with it. **A reference
  stream's payload read as itself and wrote back byte for byte through the
  harness** — which is the claim worth making, since the syntax has more than
  one equal-length spelling and "a decoder accepts it" would not distinguish
  them; the unit tests pin a payload this crate writes. On a
  sixteen-channel stream carrying it alongside the matrices and the dynamic
  range word, the decoder's own account of the programme agrees with what was
  written. See [docs/objects.md](docs/objects.md).
- 🔴 **The oracle has a version, and it costs 13.4 GiB.** `truehdd` 0.4.0 — the
  build installed here — reads the six-bit object gain index *twice*, so a
  payload stating a gain that way runs it off the end and it abandons the
  access unit; 0.6.1 reads it once and needs a newer compiler than this machine
  has. So the harness runs whatever `HARLETTY` names — `harletty`, the
  maintained line of `truehdd`, is the decoder now; `TRUEHDD` is still
  honoured — and under
  `--features oracle` reads every payload back with the `truehd` library in
  process as well. That feature is off by default: compiling `truehd` alone
  peaks at 13.4 GiB in one rustc, against a hosted runner's 7 GB.
- ✅ **Interpolated matrix coefficients**: sync word C's step per access unit,
  which ramps within a unit and accumulates at the end of one, so a single
  statement tracks a trend for as long as the matrix stands. Worth **−0.42 %**
  on a stream whose correlation sweeps and nothing either way on one whose does
  not. The alternative was measured first and loses everywhere: restating the
  matrix every unit costs +17.8 % on correlated pairs, and on a still
  correlation it codes *worse* (8.3 bits a sample to 8.8), because switching a
  matrix makes the prediction filter's carried history wrong.
- 🔴 **Only half the pairs can use it.** A matrix declared by more than one
  substream has to be declared the same way in each, or a decoder stopping at
  an earlier presentation reconstructs different audio; only sync word C can
  state a step, so only a destination no earlier substream describes may move.
- 🔴 **The narrow presentations are matrices, computed at decode** (read from
  a shipped stream with `harletty info --matrices`, 2026-09-07). Its immersive
  substream's matrices are lifting steps, a lossless transform of the
  elements; the three substreams below carry dense rows that *compute* the
  2.0 / 5.1 / 7.1 folds from the internal channels, constant across each
  restart interval, with the internal channels permuted so each presentation's
  substreams hold what its fold needs. This encoder's presentations are copies
  of the leading elements — a 7.1 decoder of its stream hears no objects — and
  carrying a fold as audio instead was measured at +8 %. Writing it the
  reference's way is the next piece of A2. It needs multi-source matrices,
  which are also the strongest lead on the remaining 1.3 %. See
  [docs/mlp.md](docs/mlp.md#what-a-shipped-streams-matrices-say).
- Still to write: the 8ch / 6ch / 2ch presentation metadata with their DRC
  profiles, `legacy_authoring_compatibility`, `spatial_clusters` ∈
  {12, 14, 16}.
- ✅ **Object clustering, first pass** — [`hz-cluster`](crates/hz-cluster), and
  before it the number that judges one. Rendering is linear in the object
  signals, so the whole difference between a scene and its clustering is a
  difference of gain vectors per object, measurable from the metadata and the
  audio's levels with no listener and no reference engine. The anchor holds:
  with as many elements as objects the fold costs `5.9e-8`.
  See [docs/clustering.md](docs/clustering.md).
- ✅ **Three weightings, and the third wins on every axis.** Rendering is
  linear in the object signals, so choosing the weights is a least-squares
  problem — solved by non-negative least squares against an assumed layout,
  since the encoder cannot know the real one. Folding 48 objects into twelve:
  **0.065 mean / 0.439 worst** against 0.239 / 0.538 for panning onto the
  elements and 0.175 / 0.628 for taking the nearest. It reaches the floor —
  what the fold would cost if the encoder *knew* the layout — for a point and
  for a width alike.
- 🔴 **Which reference is not a detail.** Fitted against a uniform sphere and
  measured on 7.1.4, a wide object costs 0.784: barely better than ignoring its
  width. Fitted against an ordinary 7.1.4 and measured on 5.1 and 7.1, which are
  not it: 0.121 and 0.137. Panning with a width is not the same operator on two
  layouts, so assuming something ordinary transfers and assuming something
  uniform does not.
- 🔴 **A defect the first measurement could not see.** Panning objects onto the
  elements renders them **16 to 25 % too loud** — one to two decibels. Its
  weights carry unit power, which is what the panning guarantees and what the
  metric checked; the elements are then panned again and add coherently. The
  metric now reports the level *as rendered*: a proxy a weighting is built to
  satisfy is not a check on it.
- ✅ **And it is cheap.** 0.63 µs per object per block, linear, with no fixed
  cost worth naming: **10.7 s of processor time for a two-hour programme** at
  a hundred objects and twelve elements, 15.4 s at sixteen. The TrueHD encoder
  it feeds takes 21.5 minutes for the same programme, so the clustering is
  0.8 % of it.
  Of the work, the solve is under a third; panning onto the reference is the
  larger half. The reference itself was being rebuilt every block — 24 µs of
  pure waste, a third of the work on a small scene — and is now held across
  them, which is what `Clusterer` is for.
- 🔴 **A width is easier to fold than a point** (0.237 against 0.530 at the
  floor), which is the opposite of what one expects: a broad target is something
  point elements can add up to, and a narrow one between them is not.
- 🔴 **Elements have to stay still.** The first measurement of a scene turning
  by 3° moved eleven of twelve elements by 3° and the twelfth by **9**, because
  one object crossed a boundary and dragged its element after it. Fixed by
  carrying the assignment between blocks with a five-degree margin: 0.27° a
  block over 150 blocks, no jump over fifteen degrees.
- 🔴 **Nothing shipped carries an object's size.** Counted across every master
  to hand, `size` is `0.0` every time, and `size3D`, `decorr`, `snap`, `zones`
  and the screen anchor are never stated at all. Only position and gain move.
  The format *can* state a size, so this is the reference encoder's choice
  rather than a limit; what follows for us does not depend on why. **A mix
  uses size and the output will not carry it, so the fold is the only place
  left for it to go.**
- 🔴 **And no weighting carries one yet.** The metric now pans the object with
  its size and the elements without, which is what the format does — before
  that it could not see a width at all. On a ring of sixteen into twelve, the
  same object costs 0.638 as a point, 0.898 spread over the elements once it
  has a width, and 0.613 flattened onto one. A third weighting is needed,
  fitted to the object's directional energy rather than to its direction;
  `a_width_should_survive_the_fold` states the target and is ignored until it
  is met.
- 🔴 **Real trajectories broke it on the first attempt**, and synthesised ones
  never would have: a scene whose objects have two directions between them
  leaves most of the elements piled on top of each other, and a triangulation
  over coincident points has none to find. Elements the scene did not ask for
  are now moved where the geometry can use them, loudest-stays-put.
- 🔴 **Still nothing to compare against, and the reference engine would not
  help.** Every master to hand is the *output* of a clustering — eleven,
  thirteen or fifteen objects, which is twelve, fourteen or sixteen elements.
  There is nothing with more objects than a bitstream carries, so there is
  nothing to hand two clusterers. What unblocks it is **authored content**: a
  mix with sixty objects, from a renderer rather than from a decoder.
- **Accept when**: (a) the stream decodes in `harletty` with the object
  count and trajectories we encoded, events comparing equal to the source
  master; (b) rendering it through an independent renderer and rendering
  the source master directly differ by less than a stated
  spectral/positional metric;
  (c) it plays as immersive audio on real decoder hardware; (d) a pipeline
  driving it from a job description gets a usable track.
- **Size**: 2–4 weeks for the bitstream, open-ended for clustering
  quality. **Risk**: high on clustering, low on framing.

### Track B — lossy

#### Phase B1 — AC-3 encoder

The warm-up that builds the MDCT / exponent / bit-allocation machinery
E-AC-3 needs. Port from FFmpeg `ac3enc`, then Rust-ify.

- **Accept when**: output decodes cleanly in FFmpeg and in the workspace
  decoder; quality at parity with `ffmpeg -c:a ac3` at equal bitrate on
  real material; metadata (dialnorm, DRC, downmix flags, `bsmod`) matches
  what the reference engine writes for the same job.
- **Size**: 2–3 weeks. **Risk**: low.

#### Phase B2 — E-AC-3

- Substreams, 5.1 and 7.1, the enhanced coupling and transform tools that
  matter at delivery bitrates, dependent-substream pairing for >5.1.
- Note the known trap already recorded in this workspace: dependent
  substreams must be paired with their independent substream, not dropped.
- **Accept when**: decodes in the workspace `eac3` crate and in FFmpeg;
  legacy 5.1 decoders get a clean core; bitrate/quality compared against
  the reference engine at the same settings.
- **Size**: 3–4 weeks. **Risk**: medium.

#### Phase B3 — Joint object coding

- TS 103 420 defines the *bitstream*, not the encoder. The new work is
  parameter estimation: given N objects, produce the 5.1 core downmix plus
  the per-band mixing matrices that best reconstruct the objects, with the
  quantisation and temporal smoothing the spec allows.
- **Accept when**: the workspace `eac3` decoder reconstructs objects whose
  rendered output is close to the source objects by a stated metric;
  legacy decoders still get the clean 5.1 core; A/B listening against the
  reference engine's output at the same bitrate.
- **Size**: 4–8 weeks. **Risk**: high — this is genuine DSP research.

### Phase 6 — AC-4

- Build on `oxideav-ac4` (MIT, already in the workspace as a reference
  source): it has framing, the table of contents, and a channel-based
  encoder. New work is the immersive presentations and object substreams.
- **Size**: unscoped until Track B lands. **Risk**: high.

### Phase 7 — Job layer

- Native job format: TOML, readable, with sane defaults.
- Progress on standard error for a host driving a bar — what `encode
  --progress` already does — and a log file.
- **Accept when**: a pipeline runs a whole title through the binary from a
  job description and gets a playable immersive TrueHD track — that is
  checkpoint 2.
- **Size**: ~1 week once an encoder exists. **Risk**: low.

---

## 5. Validation strategy

Four independent oracles, used together so no single bug can pass:

1. **Bit-exact round-trip** (lossless track only, but decisive):
   `decode(encode(x)) == x` through two independent decoders.
   *Input has to be produced first*: a master set with its audio component
   in place, or the 7.1.4 channel-check sample and `dumps/`. Budget decode
   time and disk for this at the start of phase A1, not in the middle of it.
2. **Metadata round-trip**: encode a master, decode it back to a master,
   compare events with the existing comparison helper in `damf`.
3. **Differential vs the reference engine**: same input, same job
   settings, compare decoded PCM, bitrate, and metadata fields. From a
   licensed run only — see LEGAL.md §3.1.
4. **Third-party decoders and real hardware**: FFmpeg, and playback on an
   actual AV receiver, because a stream that satisfies every offline
   check and that no receiver will lock onto is still a failure.

Plus listening. The user of this workspace listens to results live; no
phase is "done" on metrics alone.

The harness (`cargo xtask corpus`) runs 1–3 over a directory of master sets
and writes a report per phase, so a regression anywhere shows up as a delta
in the table rather than as a surprise months later.

---

## 6. Risk register

| Risk | Impact | Mitigation |
|---|---|---|
| Object clustering quality below the reference engine | Audible; the immersive presentation is the product | Treat as research with its own crate and metric; ship a documented "our clustering" rather than pretending parity |
| No clustering *input* data: every master to hand is post-clustering output (11/13/15 objects = 12/14/16 clusters) | `hz-cluster` cannot be evaluated on real material | Source authored ADM BW64 masters, or synthesise scenes with known ground truth. Budget this inside phase A2 |
| Joint object coding parameter estimation | Same | Sequence it after Track A so the project has already shipped something useful |
| Patent exposure | Legal, not technical | Publish the notice, take the posture of comparable projects |
| Scope drift into container/muxing/IMF work | Never finishes | Out of scope: use FFmpeg/mkvmerge for muxing. This project produces elementary streams and master files only |
| A measurement taken from an unlicensed run of the reference engine | Fatal, project-ending | Written rule in LEGAL.md §3.1; nothing enters the repository from one |

---

## 7. Immediate next steps

1. ~~Phase 0: scaffold the `hz-*` workspace, CI, and the harness.~~ Done.
2. ~~Phase 1: master-file and BW64 readers, the projection, `convert`.~~ Done.
3. Bring real material to hand, so phase A1 has bit-exact round-trip
   material waiting for it rather than being blocked on decode time.
4. ~~Finish phase 2: the speech gate for dialnorm.~~ Done, and calibrated
   against ground truth we built. What still needs the reference engine — the
   gate's delta and the compression curves — needs a **licensed** copy of it
   first (LEGAL.md §3.1).

## 8. One thing the decoder's own output cannot do

`convert` refuses the master set produced by decoding the local 7.1.4 sample,
and it is right to. The set declares sixteen elements — one bed channel and
fifteen objects — over a twenty-one channel audio component. Mapping those
onto each other means choosing five channels to ignore, and any choice
silently mislabels every track after it.

Every master to hand shows the same shape: exactly one bed channel. That is
the decoder's projection under-declaring the bed, not the format, and it is
worth fixing on that side — a master set whose config and audio disagree
cannot be converted by anything without guessing.
