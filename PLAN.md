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
  hz-dca        DTS core, EXSS and XLL writers + the DTS:X extension
  hz-ac4        AC-4, built on oxideav-ac4
  hz-iamf       IAMF sequences: OBUs, layouts, FLAC / LPCM / Opus, Matroska
  hz-programme  what every writer does before its own format: the master's
                tracks and object paths, renders, fold / overlay mixing,
                sample coding, progress — no codec crate
  hz-job        job model: TOML
  hz-cli        the binary
xtask/          development harness, differential runs, report generation
```

Dependency rule: `hz-ac3`, `hz-mlp`, `hz-dca`, `hz-ac4` never depend on each other;
they all depend on `hz-core`, `hz-analysis`, `hz-render`. `hz-programme`
holds what the writers share and depends on no codec crate. `hz-cli` is the
only crate allowed to know about all of them.

---

## 3. Three independent tracks

After the shared phases 1–2, the lossless and lossy work share almost
nothing — MLP is prediction + entropy coding, AC-3/E-AC-3 is MDCT +
bit allocation. They can be developed in either order or in parallel.

```
  Phase 1  I/O + conversion  ─┬─► Track A: Phase A1 MLP lossless ─► A2 immersive TrueHD
  Phase 2  analysis + presentations ─┴─► Track B: Phase B1 AC-3 ─► B2 E-AC-3 ─► B3 joint object coding
                                                                     └─► Phase 6 AC-4
                              └─► Track C: C1 DTS core ─► C2 lossless (XLL) ─► C3 DTS:X 7.1.4 ─► C6 objects
  Phase 7  job layer  (needs whichever track is finished)
```

**Chosen order: 1 → 2 → Track A → Phase 7 → Track B → 6.**
Track A reaches checkpoint 2 and it has the
strongest objective success criterion in the whole project: a lossless
codec either round-trips bit-exactly through an existing decoder or it
does not. Track B has the wider audience (streaming delivery is E-AC-3
with joint object coding) but the fuzziest pass/fail.

Track C (DTS) shares nothing with Track B and can run beside it; its
pass/fail is as sharp as Track A's, except for the receiver test of C3.

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

#### Phase B0 — Groundwork — **done**

- The Evolution frame moved from `hz-mlp` to [`hz-meta::emdf`](crates/hz-meta/src/emdf.rs),
  so the E-AC-3 writer can use it without depending on the TrueHD crate. It
  models the whole payload configuration and the protection lengths, and reads
  the frame behind E-AC-3's sync word. TrueHD streams are byte-identical.
- `cargo run -p xtask --features eac3-oracle -- eac3 <streams…> [--dump DIR]`
  reads reference streams through the workspace `eac3` decoder (an optional
  harness dependency, like `truehd`), reads every Evolution frame again with
  `hz-meta`, and rewrites it. On six local streams, 11 085 frames: all read as
  the oracle reads them, all rewritten bit for bit up to the protection fields.
  `--dump` keeps every payload (`<stem>.emdf`), for the byte-for-byte gates
  of B3.
- What the reference streams carry, which B2 and B3 now have to write:
  - a single **independent 5.1 substream** (acmod 7 + LFE) at 640 or 768 kb/s;
    the 7.1 streams to hand are a legacy AC-3 core plus a dependent substream,
    with no Evolution frame at all;
  - one Evolution frame per syncframe, in the auxiliary data, carrying
    programme loudness (id 1), programme information (2), object metadata (11)
    and joint object coding (14);
  - object metadata and joint object coding **in group 0**, the second frame
    aligned; loudness with a duration of 1536 samples; none with a sample
    offset — the configurations `cargo xtask eac3` prints;
  - protection fields of **32 + 8 bits** — whose computation is still unknown
    (see the risk register);
  - downmix configurations 0 and 3, five downmix channels, 11 or 15 objects
    (12 or 16 elements with the LFE).

#### Phase B1 — AC-3 encoder — **encoder done; the dynamic range words left**

The warm-up that builds the MDCT / exponent / bit-allocation machinery
E-AC-3 needs. Port from FFmpeg `ac3enc`, then Rust-ify.

- `hz-ac3` ports FFmpeg's encoder whole — coupling, rematrixing, exponent
  strategy, the bit allocation and its constant-rate search — with the
  transform written from A/52; `harlettizer ac3` renders a master to 7.1,
  folds it to 5.1 or stereo and codes it. Against `ffmpeg -c:a ac3` at the same
  settings, decoded by FFmpeg, the SNR agrees to 0.01 dB at every rate and
  layout tried ([docs/ac3.md](docs/ac3.md)); every frame also decodes through
  the workspace `eac3` crate's AC-3 path (`cargo xtask ac3`, behind
  `eac3-oracle`), with the same agreement.
- Left: the dynamic range words and the dialnorm set from the measured
  dialogue level (the stream carries no DRC yet); listening.

- **Accept when**: output decodes cleanly in FFmpeg and in the workspace
  decoder; quality at parity with `ffmpeg -c:a ac3` at equal bitrate on
  real material; metadata (dialnorm, DRC, downmix flags, `bsmod`) matches
  what the reference engine writes for the same job.
- **Size**: 2–3 weeks. **Risk**: low.

#### Phase B2 — E-AC-3 — **an independent substream, carrying objects; the dependent substream left**

- `hz-ac3` writes one independent E-AC-3 substream at any rate up to 1024 kb/s
  (`harlettizer ac3 --eac3`), within 0.11 dB SNR of `ffmpeg -c:a eac3` at every
  rate tried, and decoding strictly in FFmpeg and through the `eac3` crate
  ([docs/eac3.md](docs/eac3.md)).
- Every reference stream carries its Evolution frame in block 2's skip field;
  the encoder puts one there (`Encoder::carry_skip_field`).
- **The graft** (`cargo xtask graft`): a reference stream's core coded again by
  `hz-ac3`, its Evolution frames carried across byte for byte. Every frame of
  four reference streams rebuilds its objects in the workspace decoder, as
  close to the reference's objects as the core is to its core (35 to 41 dB) —
  so the frames, the skip field and the alignment are right, and B3 can start.
- Left: the dependent substream (7.1, seven-channel downmixes), spectral
  extension and the other low-rate tools, the metadata fields; and the
  receiver test the graft makes possible.


- Substreams, 5.1 and 7.1, the enhanced coupling and transform tools that
  matter at delivery bitrates, dependent-substream pairing for >5.1.
- Note the known trap already recorded in this workspace: dependent
  substreams must be paired with their independent substream, not dropped.
- **Accept when**: decodes in the workspace `eac3` crate and in FFmpeg;
  legacy 5.1 decoders get a clean core; bitrate/quality compared against
  the reference engine at the same settings.
- **Size**: 3–4 weeks. **Risk**: medium.

#### Phase B3 — Joint object coding — **the payload done; a first estimator**

- `hz-joc` reads and writes the payload of TS 103 420 clause 6.2. Every one
  of the 10 189 payloads of the five local streams writes back byte for byte
  and reads field for field as the workspace decoder reads it
  (`cargo xtask eac3`); see [docs/joc.md](docs/joc.md).
- The reference streams code every object one way: twelve parameter bands,
  fine quantisation, dense coefficients, one data point a frame, a steep slope
  for some. That is what the estimator writes first.
- `hz_joc::estimate` fits the gains to the decoder's output: each downmix
  channel's share of each band synthesised through the decoder's QMF bank
  (`hz_joc::qmf`), an object's sixty gains fitted together under a frame-long
  Hann window. Refitted to a reference's own core and objects
  (`cargo xtask graft --refit synthesis`), 93.6 to 99.9 % of its gains land on
  the reference's step, and its objects come within 4–5 dB of what the
  reference's own gains rebuild on slid frames — 3 to 4 dB better than the
  first, band-by-band fit in the QMF domain, which could not see the
  subbands' leakage. It steps an object's gains at a slot, rather than ramp
  them, where the frame's error says so (the steep slope): mostly where the
  reference steps, at the same slot, gaining 3 to 4 dB on the stepped frames
  of film. A budget keeps each payload within what the skip field leaves it,
  quantising gains for bits only where a frame is over (`--skip-limit`
  exercises it: the 60-second film loses 0.02 dB at 330 bytes). Next: fit
  to a core rendered from a master.


- TS 103 420 defines the *bitstream*, not the encoder. The new work is
  parameter estimation: given N objects, produce the 5.1 core downmix plus
  the per-band mixing matrices that best reconstruct the objects, with the
  quantisation and temporal smoothing the spec allows.
- **Accept when**: the workspace `eac3` decoder reconstructs objects whose
  rendered output is close to the source objects by a stated metric;
  legacy decoders still get the clean 5.1 core; A/B listening against the
  reference engine's output at the same bitrate.
- **Size**: 4–8 weeks. **Risk**: high — this is genuine DSP research.

### Track C — DTS

DTS-HD Master Audio with the DTS:X extension. Like Track A it is lossless
where it matters, so it has the same decisive pass/fail: the lossless layers
round-trip bit-exactly through an existing decoder or they do not. It shares
nothing with Tracks A and B beyond `hz-programme` and `hz-render`.

What the decoding side already knows, and why the track is possible:

- The `dca` crate (harletty-bridge) decodes the core, the extension
  substream (EXSS), the lossless extension (XLL) and the DTS:X extension —
  bit-exactly against FFmpeg wherever FFmpeg decodes at all. It is the oracle
  for every phase below, as `truehd` was for Track A.
- **The standard DTS:X profile, a fixed 7.1.4, is read completely**: a
  lossless 7.1 bed into which each height is folded at −3 dB
  (`rmul15(h, 23170)` into L, R, Lb and Rb), then, at the end of the XLL
  frame, a 22-byte prefix (a sparse type-2 fold matrix under a CRC16) and a
  bare four-channel XLL channel set carrying TFL, TFR, TBL and TBR, each
  full-coded. A 64-bit word in the EXSS asset descriptor locates the block.
- The profiles that carry objects are read only in part — declarations,
  positions, stated fold rows — and several of their fields are pinned to
  the values observed rather than understood.
- No open DTS encoder exists beyond FFmpeg's `dcaenc.c` (LGPL-2.1-or-later),
  which writes the core only, six channels at most.

**Order: C0 → C1 → C2 → C3, then C4 and C5. C6 starts only once C3 has
played on hardware.**

#### Phase C0 — Groundwork — **done**

- ✅ **`hz-dca`**: CRC16, the core frame header, the extension substream
  header and its asset descriptors, the DTS:X navigation word, and a stream
  cut into frames — each a model that keeps every field as coded, a reader,
  and a writer. Descriptor tails and the reserved bits before the header
  checksum are carried as raw bits; `ExssHeader::fit` sizes a header written
  from scratch, and the checksum is always computed, never copied.
- ✅ **`cargo xtask dts`** reads every header of a stream and writes it back.
  On 42 local streams of every shape to hand — plain core, lossless 5.1 and
  7.1, the standard DTS:X profile, the alternate profiles, a lossy carrier —
  every core header and every substream header comes back byte for byte,
  with every checksum valid — 416,543 core and 415,792 substream headers.
  With `--features dca-oracle` (an optional harness dependency, like
  `truehd`), the `dca` decoder reads the same frames: it decodes every one,
  and its core header fields agree with ours on every one.
- ✅ **The navigation word's size field is eleven bits, not ten.** Blocks
  over 4,116 bytes carry the eleventh bit in what had been read as the last
  bit of a `0x8a28` marker. On the four standard-profile streams to hand,
  21 % of one stream's frames have a block that large (up to 5,056 bytes);
  read with eleven bits, the word is found in every frame of all four and
  points at exactly the block the decoder locates — 34,311 of 34,311. The
  decoder's own reader takes the ten-bit form and finds those blocks by its
  fallback instead. The alternate profiles carry no navigation word at all
  (367,405 frames with an extension block, none with a word).
- ✅ **The integer unfold** is in the harness: `bed − ((h · 23170 + 2^14) >>
  15)` per corner, the oracle C3 is held to. Measured on the same four
  streams, the decoder's floating-point presentation helper (`FoldPlan::clean`)
  differs from it on 1.1–1.8 % of corner samples, by one LSB each time — so
  that helper is not the oracle for C3, as expected.
- ✅ LEGAL.md: ETSI TS 102 114 V1.6.1, the FFmpeg DTS files and the `dca`
  oracle are listed in §2, with the rule that the DTS:X extension comes from
  stream measurements and from nothing the format owner distributes. §5's
  patent notice already speaks of the formats implemented in general terms;
  whether DTS wants more than that is still the maintainer's to decide.
- Fixtures synthetic, as everywhere else.

#### Phase C1 — Core encoder (5.1, lossy) — **done**

- ✅ **`hz_dca::encoder`**: `dcaenc.c` ported stage for stage — the 32-band
  polyphase analysis, the 64× LFE decimation, the masking model, the SNR-offset
  search, Huffman, block or plain codes per allocation, optional subband ADPCM
  (`dcaadpcm.c`) — with the fixed-point MDCT `av_tx` gives it
  (`hz_dca::mdct`, from `libavutil/tx_template.c`). Everything FFmpeg's encoder
  takes: mono, stereo, 2.2, 5.0 and 5.1, 8 to 48 kHz, 32 to 3840 kb/s.
  `Config::reference_five_one()` is the reference streams' frame: 5.1 at
  48 kHz, 1509 kb/s, 2012-byte frames, rate code 24 (1536 kb/s). The header is written through
  `CoreHeader`. No allocation per frame; ten seconds of 5.1 encode in 0.25 s,
  2.0 s with ADPCM — FFmpeg's own times are 0.23 s and 2.2 s.
- ✅ **It is FFmpeg's stream, byte for byte.** Against `ffmpeg -c:a dca`
  (n9.0.2) on the same samples, with FFmpeg's LFE history reproduced: 16 of 17
  synthetic configurations identical — 5.1, 5.0, 2.2, stereo and mono; 8, 16,
  22.05, 32, 44.1 and 48 kHz; 128 kb/s to 3840 kb/s, and 5.1 down to 700
  kb/s, near the 670 kb/s it accepts at all; ADPCM on and off; silence,
  full-scale squares, full-scale noise. The
  seventeenth, full-scale white noise in all six channels, differs by 2 bytes in
  2 of 563 frames, inside the LFE block: FFmpeg reads one sample *before* its
  input buffer there (below), so its output depends on memory it does not own.
  The MDCT is libavutil's to the last bit on all 1024 outputs checked. Two
  hashes of FFmpeg's streams, from an integer-synthesised fixture, are a test.
- ✅ **One defect of FFmpeg's is fixed, and optional.** Its LFE history is
  filled from a channel-order entry of −1: the previous sample's last input
  channel, the right surround. So half of each LFE output's window sees the
  right surround of the previous frame. Fixed by default; the LFE, a 60 Hz tone
  beside a chirp in the right surround, comes back at **11.2 dB SNR instead of
  2.6**, and the streams differ in nothing but the 72-bit LFE block.
  `Config::lfe_history_quirk` puts the defect back, for the comparison.
- ✅ **A second defect, found in review, is fixed outright.** FFmpeg's
  encoder takes the lossless step sizes when the rate *code* is 3; code 3 is
  96 kb/s, and FFmpeg's decoder compares the rate the code names (lossless is
  code 31). With ADPCM the encoder's predictor history then follows steps no
  decoder uses: a 440 Hz tone, mono at 16 kHz and 96 kb/s, comes back from
  FFmpeg's own stream at −3.0 dB SNR and from this one at 80.1 dB. Without
  ADPCM the step size is never used and the streams stay identical. A test
  decodes that case through FFmpeg where it is installed.
- 🔴 **`dca` has the same confusion, in its decoder.** It reads rate code 3 as
  lossless and dequantises every subband with the wrong steps, so any 96 kb/s
  core decodes wrong through the oracle — 0.4 dB SNR on a stream FFmpeg's
  decoder takes at 80.2 dB, ADPCM or not. At that rate the oracle is not one;
  the fix belongs in harletty-bridge.
- ✅ **It decodes cleanly in both decoders.** `dca` (the oracle) decodes every
  frame of all 17 streams, and FFmpeg reports no error on any.
  `cargo run -p xtask --features dca-oracle -- dts-encode in.s32 --channels 6
  --out out.dts` encodes, rewrites every header and decodes through `dca`; `dts
  --reference` does the same for any core stream. Both decoders put the output
  512 samples behind the input and agree on every channel's SNR to 0.01 dB. On
  ten seconds of 5.1 at 1509 kb/s: L (pink noise) 23.5 dB, R (440 Hz) 66.0,
  C (beeps) 76.2, Ls (white noise above 3 kHz) 14.0, Rs (gated chirp)
  69.1.
- **What the port measured about `dcaenc`, for the better core after C3:**
  the LFE is decimated through the decoder's own interpolator, so its band
  limit applies twice — −0.3 dB at 20 Hz, −2.8 dB at 60, −11.5 dB at 120 —
  which is what limits the LFE above (54 dB once that response is taken out);
  noise-like material sits at 14–24 dB; one subframe of two subsubframes,
  every subband active, no joint intensity, no VQ, no transient modes.
- **Header fields the reference streams set differently**, measured on the
  32 local streams with a 2012-byte core: they carry `aspf` 1 (FFmpeg: 0) and,
  from a 24-bit source, `pcmr` 6 (FFmpeg: 0; two of them carry 0). The word
  length is `Config::pcm_resolution_code`; `aspf` 1 means a DSYNC after each
  subsubframe, sixteen bits FFmpeg's layout does not spend, and is left as
  FFmpeg has it.
- ✅ **Not in this phase, done in C2a: the 5.1 fold of the 7.1 bed.** The
  core carries the rears folded into the side surrounds at the coefficient the
  XLL layer declares; `hz-dca` read no XLL channel set then, and the `dca`
  crate does not expose the coefficients it parses. The fold landed with C2a,
  which writes them (below). Nor is there a `harlettizer` command: one would
  be `hz-cli`'s `ac3` path with this encoder behind it, and it is worth more
  now that C2 puts the lossless layer beside the core (C5).

#### Phase C2 — EXSS and XLL, lossless 7.1 / 5.1 — **done**

- The channel-set hierarchy the reference streams use: a six-channel set and
  a two-channel set whose downmix is embedded in the first (7.1 → 5.1);
  adaptive prediction, pairwise decorrelation, Rice coding, segments and the
  navigation table; one frequency band, 48 kHz.
- **C2a**: every channel full-coded, independent of the core. Lossless
  whatever the core does, so C1 and C2 do not wait on each other.
- **C2b**: residual against the core, for bitrate. The encoder then has to
  reconstruct the core exactly as the decoder does — its fixed-point path —
  so a core decode runs inside the encoder loop. ✅ Done, below.
- **Accept when**: bit-exact through `dca` and FFmpeg on every frame and
  every channel, at 16 and 24 bits. A 7.1 DTS-HD Master Audio stream from a
  master is useful on its own.
- **Size**: 3–5 weeks. **Risk**: medium — no encoder to read from, only the
  specification and the decoder.

What C2a did:

- ✅ **`hz_dca::xll`**: the lossless frame's headers — the common header, the
  channel set sub-headers with their downmix matrices and per-band predictor
  fields, the navigation table — as a model that keeps every field, with a
  reader and a writer; checksums computed, the reserved bits before each kept
  as raw bits, the fields FFmpeg refuses (replacement sets, speaker
  positions, extension bands) read as Table 8-4 lays them out. `cargo xtask
  dts` now reads and rewrites every lossless frame too, putting back together
  the frames peak-rate smoothing spreads over two substreams. On the 43
  local streams, **all 409,072 lossless frame headers** of the 39 that carry
  them come back byte for byte, every checksum valid — channel sets of 6+2 on
  the 7.1 streams, 6 on the 5.1 ones, 2 on a 96 kHz stereo one; 49 frames of
  the 16-bit 5.1 reference stream span two substreams. The core and substream headers of
  all 43 still rewrite byte for byte (422,168 and 421,417), and the oracle
  decodes every frame.
- ✅ **The reference streams' shape**, measured with that reader and copied.
  A 7.1 is a six-channel set — C, L, R, LFE and the side surrounds Lss/Rss,
  mask `0x627` — and a two-channel set of the rears Lsr/Rsr (`0x180`) whose
  downmix is embedded in the first. Every row of its matrix scales the 5.1
  by −3 dB and each rear folds into its side at −3 dB, so a decoder of both
  sets subtracts `(rear · 32767 + 2^14) >> 15` from the side. Two segments of
  256 samples, the MSB/LSB split flag on with nothing split (the 16-bit 5.1
  reference stream leaves it off and uses eight segments of 64), no band checksums,
  frames padded to a 32-bit word. A 5.1 is one set, C L R Ls Rs LFE (`0x3f`).
- ✅ **`hz_dca::lossless`**: PCM to lossless frames, every channel coded as
  itself. No encoder exists to port, so it is the decoder run backwards —
  §8.5, and FFmpeg's `dca_xll.c` rounding for rounding: the embedded
  downmix; pairwise decorrelation, the pairing and coefficients that save
  most under each channel's cheapest fixed predictor; prediction per channel,
  fixed of order 0 to 3 or adaptive of order 1 to 15 from a windowed
  Levinson–Durbin fit, its reflection coefficients quantised to the format's
  table and turned into the direct form exactly as the decoder turns them
  (an order whose arithmetic would leave 32 bits is refused, since C and
  Rust decoders would wrap it differently); and per segment Rice or binary
  codes, per channel or shared, or the previous segment's again. Every choice
  is priced exactly and the cheapest taken. Input with bits below the stated
  resolution is refused, naming the sample.
- ✅ **The fold**: the core is made from the 5.1 a decoder of the first set
  alone presents — every channel at −3 dB, each side with its rear — so a 7.1
  input now has a 5.1 core and a core-only player hears the same mix as a
  5.1 lossless one. A side and its rear both at full scale pass 24 bits
  inside the first set, which the format allows (the decoders' arithmetic
  wraps there and undoes it exactly); the core's copy is clipped.
- ✅ **`hz_dca::master`**: DTS-HD Master Audio frame by frame — the C1 core
  (5.1 at 1509 kb/s, the word-length code the reference streams carry) and an extension
  substream whose one asset is the lossless layer, with the reference
  streams' substream header (one presentation, the core backward-compatible,
  component mask `0x201`, speaker activity `0x84b` or `0x00f`, the embedded
  six-channel flag on a 7.1) and no smoothing buffer stated: every lossless
  frame sits whole in its own substream.
- ✅ **Bit-exact through both decoders, every frame and every channel.**
  `cargo run -p xtask --features dca-oracle -- dts-encode in.s32 --channels 8
  --lossless 24 --out out.dts` encodes, rewrites every header, decodes
  through `dca` and compares its integers with the input, then decodes
  through FFmpeg (n9.0.2, `-err_detect crccheck`) and compares again; FFmpeg
  has to report DTS-HD MA and print nothing. Ten seconds of real 7.1 decoded
  from a reference stream, at 24 bits and requantised to 16, and its 5.1:
  all 480,000 samples of every channel identical through both, 938 frames
  each. Synthetic full-scale white noise, full-scale squares that overflow
  the fold, alternating extremes, silence and a mix, 5.1 and 7.1 at 16 and
  24 bits: identical. `master_audio_comes_back_bit_for_bit` holds the four
  configurations, and two shapes off the reference streams' (`--segments`,
  `--lsb-split`: eight segments without the split flag at 24 bits, 256 of two
  samples with it at 16), through `dca` with the feature and FFmpeg where
  installed; the crate's own tests decode with a mirror of FFmpeg's reader,
  so CI checks the round trip without either. One LSB added in the fold fails
  both.
- **What it costs.** On that material the lossless layer alone is 49.5 % of
  the 24-bit 7.1 PCM, against 46.7 % for `flac -8` on the same samples;
  5.1/24 47.4 % (FLAC 45.4), 7.1/16 28.8 % (26.2), 5.1/16 27.1 % (26.0). The
  gap is mostly the format's: frames of 512 samples whose predictors restart
  every frame, the first samples of each coded raw. Full-scale noise costs
  102–103 % of its PCM, 10.9 Mb/s for 7.1/24 with the core — under the 24.5 Mb/s
  ceiling at 48 kHz whatever the material; what C4 has to manage is the
  buffer, not the peak frame. The lossless layer encodes at 18× real time for
  7.1/24 (0.54 s for ten seconds), 12× with the core; the sample paths do not
  allocate per frame (the small header model is rebuilt each frame).
- **What the streams and FFmpeg say that the specification does not.** The
  navigation table's pseudo-code (§8.4.2) reads a segment size before each
  segment's channel set sizes; neither FFmpeg nor any stream has one.
  Neither `dca` nor FFmpeg checks the lossless headers' checksums by default
  (FFmpeg does with `-err_detect crccheck`), so this crate's reader does and
  the test asks FFmpeg to. `dca` ends a Rice code's unary part at 2^20; the
  encoder keeps every quotient at most 2^16.
- 🔴 **Not written in C2a**, none of it needed for lossless: hybrid Rice codes
  (isolated outliers in binary), the MSB/LSB split, the stereo downmix a
  primary set can carry (the reference streams carry none), and peak-rate
  smoothing (written in C4). The first two would save bits on some material; C2b, the
  residual against the core, is where the bitrate is.

What C2b did:

- ✅ **`hz_dca::core_decode`**: the core as a decoder reconstructs it, on the
  fixed-point path a lossless decoder adds its residuals to
  (`ff_dca_core_filter_fixed`) — each subband dequantised, with the scale
  factor adjustment of Huffman-coded ones and plain words read back signed;
  ADPCM undone from the decoder's own history, clipped to 24 bits; the
  32-band synthesis through `dcadct.c`'s fixed-point cosine modulation and
  `synth_filter.c`'s 512-tap window; the LFE scaled at its 0.035 step and
  interpolated 64 times. It reads the frame from the encoder that wrote it —
  allocations, scale factor indices, predictors, quantised values — rather
  than parsing the bytes, and handles what that encoder writes. **It is
  FFmpeg's decode to the last bit**: against `ffmpeg -flags +bitexact`, which
  takes the same path, every sample of every channel, LFE included, with
  ADPCM and without, and on full-scale noise and squares where the clips and
  the cosine modulation's input scaling engage; hashes of FFmpeg's output
  stand in for it where it is not installed. About 36 kB of state, nothing
  allocated per frame.
- ✅ **Residual coding** (`lossless::Config::residual`, on by default;
  `dts-encode --full-coded` keeps C2a's mode). Per frame and per channel of
  the first set, the channel is coded as its difference from the core's
  reconstruction where that is cheaper — both priced with the full predictor
  search — exactly inverting `combine_residual_frame`: the core brought to
  the set's resolution by `24 − bits` with rounding, for a 7.1 first raised
  by the fold scale's Q16 inverse and clipped to 24 bits, and added *before*
  the embedded downmix is undone, so the difference is taken from the channel
  with its rears in it. The side surrounds of a 7.1 are coded against the
  core's surrounds (`ff_dca_core_map_spkr`); the rears never are. The
  header's residual mask says which channels are.
- ✅ **The core runs a frame ahead of the lossless layer.** A decoder's core
  output is 512 samples behind its input, so the stream's frame *n* carries
  the core encoded from input frame *n* + 1 — which decodes to input frame
  *n* — beside the lossless frame of input frame *n*. The core frame made
  from the first input frame decodes to the silence before it and is not
  sent; the core encoder's subband prediction restarts from silence, as the
  decoder's does; `master::Encoder::finish` sends the last input frame with a
  core made from silence. The stream keeps one frame per input frame, and
  both layers now decode in step with the input. In C2a a core-only decode
  was a frame late; that is fixed for full coding too.
- ✅ **FFmpeg's recovery frame.** FFmpeg decodes the first frame of a stream
  that has residual channels and more than one channel set from the core
  alone (`Forcing XLL recovery mode`, as after a seek) — and so it does on
  the reference streams, whose first frame codes the LFE against the core. A
  7.1's first frame is therefore coded without the core, and the stream is
  bit-exact through FFmpeg from its first sample. `dca` has no such mode.
- ✅ **Bit-exact through both decoders, every frame and every channel.** The
  four standard-profile reference streams' beds, decoded whole by FFmpeg
  (34,311 frames, 6 min 6 s) — the 7.1 bed at 24 bits, the 5.1 its first set
  presents, both requantised to 16 — encoded with a plain and an ADPCM core,
  residual and full-coded: 64 streams. Ten seconds each of tones, pink noise,
  full-scale white noise and a mix of them with silence and a chirp, the same
  16 configurations: 64 more. Cores at 768 and 3840 kb/s instead of 1509: 12
  more. Every one identical to its input through `dca` and through FFmpeg
  (`-err_detect crccheck`), every header rewriting byte for byte with its
  checksum valid. `master_audio_comes_back_bit_for_bit` now holds 14
  configurations: 5.1 and 7.1 at 16 and 24 bits against a plain and an ADPCM
  core and coded alone, and the two off-reference shapes. The crate's own
  tests decode the residual with the FFmpeg-mirror reader as well.
- **What it saves.** The lossless layer alone, full-coded (C2a) against
  residual (C2b), plain core at 1509 kb/s; the last column is the reference
  encoder's own bed on the same streams (its lossless frames up to the
  DTS:X block the navigation word points at, which is the same 7.1 our input
  is decoded from):

  | 7.1, 24 bits, standard-profile reference stream | C2a | C2b | saved | reference bed |
  |---|---|---|---|---|
  | stream 1 | 4403 kb/s | 3447 kb/s | 21.7 % | 3618 kb/s |
  | stream 2 | 4268 kb/s | 3113 kb/s | 27.1 % | 3282 kb/s |
  | stream 3 | 3538 kb/s | 2660 kb/s | 24.8 % | 2815 kb/s |
  | stream 4 | 4675 kb/s | 3622 kb/s | 22.5 % | 3800 kb/s |

  So the residual takes a fifth to a quarter off a 24-bit 7.1, and the
  layer comes out 4.7–5.5 % smaller than the reference encoder's on its own
  material. The lower the resolution the larger the share the core already
  carries: 7.1/16 40–45 % smaller, 5.1/24 31–38 %, 5.1/16 61–75 % (252 kb/s
  for stream 2's). Synthetic: tones 30–70 %, pink noise 16–38 %, the mix
  28–70 %, and even full-scale white noise 7–18 % (101.7 % of its PCM → 94.4
  % for 7.1/24), the core's 14 dB on noise still being worth something. An
  ADPCM core changes the layer by under 2 %. The LFE is the channel least
  often coded against the core — the C1 core limits its band twice — where
  the four standard-profile streams code all six against it in every frame
  but at most one. Across the 39 lossless streams of the local corpus, every
  one codes its first set against the core: 18 all six channels in every
  frame, the other 21 choosing per frame and channel, as this encoder does.
- ✅ **`cargo xtask dts` on the 43 local streams**, again: every core,
  substream and lossless header rewrites byte for byte (422,168, 421,417 and
  409,072), every checksum valid, the oracle decodes every frame. It now also
  reports each channel set's residual mask, the lossless frames' sizes, and
  the average navigated offset — the bed's share of a DTS:X frame, which is
  where the reference encoder's bed rate above comes from.
- **What it costs.** The encoder prices each first-set channel twice, as
  itself and against the core, with the full predictor search, and decodes
  the core it sends: with the core, 7.1/24 encodes at 8× real time (28 s for
  stream 4's 3 min 35 s) against 12× full-coded. Nothing more is allocated per
  frame. And the frames are smaller at their largest too: 7572 bytes at most
  over the four beds, against 9108 full-coded — which matters to C3, whose
  navigation word cannot point past 8188 bytes.

#### Phase C3 — The DTS:X extension, fixed 7.1.4 — **bitstream done: (a), (b) and (e) hold; (c) receiver and (d) listening left**

- Render the master to 7.1.4 (`hz-programme`, `hz-render::Room`, as the IAMF
  bed does), fold each height into its lower corner at code 55, and write the
  bare four-channel set, the 22-byte prefix from its fields (not as a pasted
  constant) and the EXSS navigation word.
- ✅ **Navigation bounds** — the stated domain, below. The navigation word gives the block's offset in
  11 bits of four bytes (8,188 bytes at most) and, as C0 established, its
  size in 11 bits of four bytes plus 24 (8,212 bytes at most). Four
  full-coded 24-bit heights — 6,144 bytes of raw PCM a frame — therefore
  fit; the binding limit is the **offset**, which a full-coded bed alone can
  pass, and peak-bitrate smoothing (C4) does not shrink the logical frame.
  No offset over 7,884 bytes has been seen, so whether that field too is
  wider is open. Either a navigation form that reaches further, or a stated
  domain: a frame that cannot be navigated is rejected with an error naming
  it, never truncated or re-quantised (`Navigation::word` already refuses
  it). The bed uses the C2b residual coding here, not C2a's full-coded
  form: on the four standard-profile streams' beds its largest frame is
  7,572 bytes, under the bound, where full coding reaches 9,108.
- ✅ **Headroom** — a limiter on the composite, below. Bed plus 0.707 × height can pass 24-bit full scale. The
  fold needs the limiter (`Headroom::Limit`) or a margin before it, or the
  lossless bed clips.
- **Accept when**: (a) `dca` presents the stream as the standard height
  profile and returns the composite bed and the four height waveforms
  bit-exactly, and the harness's integer unfold (C0) turns them back into
  the twelve pre-fold channels bit-exactly; (b) FFmpeg returns the 7.1 bed
  bit-exactly and reports the DTS:X profile; (c) it plays as DTS:X on a
  licensed receiver over HDMI passthrough, with the heights where they
  belong; (d) listening; (e) a synthetic 24-bit white-noise master either
  encodes losslessly or is rejected by the navigation check, as the domain
  chosen above says.
- 🔴 The fields pinned to observed values satisfy our own decoders by
  construction. Only (c) shows that a licensed decoder accepts them.
- **Size**: 1–2 weeks for the bitstream. **Risk**: medium, all of it in (c).

What C3 did:

- ✅ **`hz_dca::dtsx`**: the extension block as a model with a reader and a
  writer. The fold matrix is a type-2 layout element: the bed's speakers
  (`0x84b`) and the output's (`0x886b`), unity level and scale, then one row
  per height of six-bit gain codes over the bed's eight columns, a byte
  alignment, a CRC16, and one byte more. The height set is a bare channel set
  (`xll::BareSet`): a sub-header in the non-one-to-one form, and a navigation
  table of its own in the frame's segments and segment-size width. After its
  data come two zero bytes, then zeroes to the 32-bit word. The fields the
  streams never vary are constants, written as the streams carry them and
  refused by the reader at any other value. **`FoldMatrix::standard()`,
  written from its fields, is the 22 bytes every reference frame carries**,
  checksum included, and its first four are the `0x02000850` decoders find
  the block by. A gain code's value is the downmix table's at `4 · code − 3`
  (55 is 23170, 61 is unity), as the `dca` crate's notes measured.
- ✅ **The reference streams through the new reader**, with `cargo xtask dts`:
  all 34,311 blocks of the four standard-profile streams read, rewrite byte
  for byte with every checksum valid, and are laid out exactly as the
  writer lays them out. That is the two-byte trailer, the padding, and the
  block ending the frame, at the first word after the bed's band data. Every
  navigation word points at the block found there. The matrix is the same
  22 bytes in every frame. The height sets are 24-bit, except 384 frames
  where the reference encoder declares 22.
- ✅ **What else a DTS:X frame carries, measured.** The asset descriptor is
  the 7.1's (eight channels, `0x84b`, the embedded 5.1), its tail two zero
  bits, the navigation word, and the padding to the byte (69 bits), on every
  frame of the four. And every substream header ends on a 32-bit word: all
  483,656 of the 64 local streams that have one, 28 to 44 bytes. C2's
  writer did not pad there, so the lossless data started off the word. It
  does now, for every layout, with reserved zeroes, so that our header is the
  reference's field for field.
- ✅ **`lossless::Layout::SevenPointOneFour`**, 24 bits only, because a
  height's share of a corner is a 24-bit quantity. The heights are folded
  into the composite 7.1 at the matrix's gains, `(h · 23170 + 2^14) >> 15`,
  before anything else, and everything C2 does is done to the composite:
  the rears' embedded downmix, the core made from it, the residual against
  that core. The heights are coded by the same search as any set, every
  channel as itself. The frame is laid out as before, then the block. The
  common header's segment-size width covers the bed's segments and the
  heights' both, since a decoder reads both tables with it. `master` writes
  the navigation word into the descriptor's tail at bit 2.
- ✅ **Headroom.** The encoder refuses a corner its height takes past 24 bits
  (`Refused::Fold`, naming frame, channel and sample): a decoder would clip
  the composite, and the unfold would no longer be exact. `harlettizer dts`
  limits before it quantises: one gain for all twelve channels, the
  `hz_cluster` limiter an element's mix has, watching the eight composite
  channels as well as the twelve, a few steps under full scale. A synthetic
  40-object scene peaked at 3.11 of full scale in the composite and was held;
  an 11-object master peaked at 0.56 and the limiter never acted.
- ✅ **The navigation domain: the word as it is read, and nothing wider.**
  The bed — the block's offset — cannot pass 8,188 bytes a frame, about
  6.1 Mb/s for the lossless 7.1. A frame past it is refused with
  `Refused::Navigation`, which names the frame and its first sample, and the
  stream ends there. It is never cut or coded coarser. No stream has shown a
  wider field, so none is assumed. What fills the bed is what the core
  cannot predict:
  - programme material fits. The four reference streams' 7.1.4 reaches 7,572
    bytes, and the reference encoder's own beds of it 7,884. A rendered
    11-object master reaches 7,716, so the margin is real but not wide;
  - **(e)**: uniform white noise in all twelve channels at 24 bits is refused
    at its first frame from a peak of 2⁻⁸ of full scale (−48 dBFS) up, the
    bed 8,600 bytes there and 9,112 at −42 dBFS. At 2⁻⁹ (−54 dBFS) and below
    it encodes losslessly, the bed 8,088 bytes at most. A test holds both
    sides: refused at −6 and −18 dBFS, lossless at −72.
- ✅ **(a) and (b) on real 7.1.4.** `cargo xtask dts --unfold` writes a
  standard-profile stream's twelve pre-fold channels: the oracle's composite
  and heights, through the integer unfold. The four reference streams'
  7.1.4, 34,311 frames and 6 min 6 s, went back through `dts-encode
  --channels 12`. `dca` presents the height profile on every frame. The
  composite 7.1 is the fold of the input, the four heights are the input's,
  and the unfold is the input's 7.1, all bit for bit. FFmpeg (n9.0.2,
  `-err_detect crccheck`) reports `DTS-HD MA + DTS:X` and returns the
  composite bit for bit. Every header, the block's included, rewrites byte
  for byte. The same on synthetic 7.1.4 with a plain and an ADPCM core
  (`the_standard_dtsx_profile_comes_back_bit_for_bit`), and in the crate's
  own tests through its FFmpeg-mirror reader, which now reads the block too.
- **What it costs, against the reference encoder on the same 7.1.4** (mean /
  largest bytes a frame):

  | Reference stream's 7.1.4 | bed, ours | bed, reference | heights, ours | heights, reference |
  |---|---|---|---|---|
  | stream 1 | 4,597 / 7,572 | 4,825 / 7,884 | 2,881 / 4,996 | 2,896 / 5,056 |
  | stream 2 | 4,151 / 6,024 | 4,376 / 6,300 | 2,815 / 3,628 | 2,824 / 3,636 |
  | stream 3 | 3,547 / 6,912 | 3,753 / 7,136 | 2,391 / 4,376 | 2,457 / 4,432 |
  | stream 4 | 4,829 / 7,448 | 5,067 / 7,776 | 3,118 / 4,540 | 3,150 / 4,524 |

  The bed is 4.7–5.5 % smaller, as C2b found on the 7.1 alone. The heights
  are 0.3–2.7 % smaller: the same coder with nothing to subtract. The whole
  stream is 43–54 % of the 24-bit PCM with the core, encoded at 6–7× real
  time.
- ✅ **`harlettizer dts`**: the master rendered as `iamf` renders it, onto
  7.1.4, limited as above, and written as a raw `.dts`, with `--adpcm` for the
  core. On an 11-object master (6,643 frames) and a 40-object scene, `dca`
  presents the height profile on every frame, every block rewrites, and
  FFmpeg reports DTS:X. Matroska and progress lines are C5. `docs/dts.md`
  has the command, the checks, the domain and the receiver recipe.
- 🔴 **(c) and (d)** need a receiver and a listener. `docs/dts.md` has the
  recipe: re-encode a reference stream's own 7.1.4 and play the two side by
  side, so that any difference is ours.

#### Phase C4 — Peak bitrate — **done**

- Peak-bitrate smoothing as the reference streams do it (their XLL frames
  vary widely in size from one frame to the next), under a ceiling of
  24.5 Mb/s with the core included; dialnorm in the core header.
  No dynamic range control to start with.
- **Accept when**: a whole title stays under the ceiling in the decoder's
  buffer model and stays bit-exact.
- **Size**: about a week. **Risk**: low to medium.

What C4 measured:

- **How smoothing works, in both decoders.** A substream's lossless data
  is a run of bytes from one stream of lossless frames laid end to end. A
  decoder with nothing buffered decodes it in place, from its first byte or
  from the sync offset the asset states (buffering it instead when a delay
  is stated), and keeps what follows the frame; with something buffered, it
  appends the run, counts the delay down and takes one frame out (FFmpeg's
  `dca_xll.c`, which the `dca` crate follows; 240 kB at most). So a large
  frame can travel partly in the substreams before its own, and no
  substream needs more than the channel allows. `hz_dca::peak::DecoderBuffer`
  keeps that buffer; `cargo xtask dts` now feeds it every substream, reports
  the rate, the frame sizes, the sync fields and what it held
  (`--rate-log` writes it frame by frame), and fails a stream on a
  substream that starves it, on sending ahead without stating a buffer, or
  on holding more than the buffer stated.
- **The four standard-profile reference streams never smooth.** Every
  lossless frame sits whole in its own substream, the sync present with
  offset 0, delay 0 and buffer code 0. They vary widely and peak far under
  the ceiling:

  | Standard-profile reference stream | average | largest frame | lossless frames: median / p99 / max, bytes | spread (sd / mean) |
  |---|---|---|---|---|
  | stream 1 | 7.33 Mb/s | 11.21 Mb/s | 9,256 / 11,968 / 12,896 | 49.0 % |
  | stream 2 | 6.94 Mb/s | 8.88 Mb/s | 7,276 / 9,312 / 9,792 | 13.2 % |
  | stream 3 | 6.20 Mb/s | 10.21 Mb/s | 6,560 / 11,056 / 11,560 | 46.0 % |
  | stream 4 | 7.70 Mb/s | 10.66 Mb/s | 8,828 / 11,332 / 12,160 | 30.2 % |

- **Feature-length streams do, and say how.** Of the 63 lossless streams
  in the local corpus, 28 state buffer code 0 and never send a frame ahead;
  34 state code 8 — a 128 kB buffer — on every frame, whether they use it
  or not, and 14 of those send frames ahead (one more, a splice, states
  both). The longest, a standard-profile
  feature of 1,125,840 frames, was measured frame by frame:
  - the schedule is the **latest** one: a frame goes whole in its own
    substream unless a later one cannot fit its own; then the substreams
    just before carry its excess, the first in part and the rest each at a
    fixed cap (12,936 bytes a frame there, 9.7 Mb/s), until the buffer has
    it. 40,882 substreams carry other than their own frame, in 1,428 runs
    of up to 1,130 frames; the buffer never holds more than 130,940 bytes
    of its 131,072. Across the corpus the fullest holds 130,988;
  - a substream in which a frame starts gives its offset, and as the delay
    **the number of substreams until that frame is due** — on all
    1,123,831 substreams that state one. Where no frame starts (2,009
    substreams) the sync is absent;
  - the delay's width is one bit up to a delay of 1, then one more than the
    bits of the delay less one (2 in 2 bits, 3–4 in 3, 5–8 in 4, 9–16 in 5,
    17–18 in 6: every delay in the corpus);
  - the navigation word describes the frame the decoder takes out of its
    buffer with that substream, not the one the substream's data starts —
    1,175,121 of 1,175,121 words across the corpus. `cargo xtask dts`
    compared it with the reassembled frame before, and so missed 17,320 on
    that stream; it now compares it with the decoder model's.
- **Dialnorm**: every core of the corpus states revision 7, code 0 —
  0 dB, none — and no asset states its own.

What C4 did:

- ✅ **`hz_dca::peak::Smoother`**: that schedule, from the frames' sizes
  alone. Each substream carries what the frames waiting need by their own
  substreams if every later substream carries all the ceiling allows, and
  at least a byte; one byte of room is kept to move a boundary off a later
  frame's first byte, where a decoder joining there would decode that
  frame in place, early (the feature does not bother: fifteen of its
  substreams start on one, stating the delay). The sync offset and delay are the feature's;
  the stated buffer is `peak::Config::buffer_code`, code 8 by default as
  there. `master` queues the frames encoded — core, lossless frame,
  navigation word — and sends each once the smoother has decided it, the
  queues keeping their memory from frame to frame. Nothing is held where
  nothing can need it: when no frame the lossless layer can write
  passes its substream's room (`lossless::Encoder::frame_bound` — a 7.1.4's
  bounded by the navigation word, any other's by the binary code that holds
  a sample in 32 bits), the stream states buffer code 0, as the reference
  streams do, and every frame goes out as soon as it is encoded. **At the
  default 24.5 Mb/s that is every layout**: the room is about 30,600 bytes, the
  largest frame 20,480 (7.1) or 16,400 (7.1.4). Otherwise the encoder looks
  `lookahead` frames ahead, 1,024 by default (eleven seconds).
- ✅ **The domain**: a frame that cannot reach the decoder in time is
  refused with `Refused::PeakRate`, which names the first such frame and
  its first sample, and says whether the substreams up to it cannot carry
  it under the ceiling (with how many bytes over) or carrying it would
  overfill the stated buffer. The stream ends before it; lossless data is
  never coded coarser. The first frame cannot be helped by any before it.
- ✅ **Dialnorm**: `encoder::Config::dialnorm`, 0 to 31 dB, written as the
  revision and code ETSI TS 102 114 table 5-20 pairs (7 and 0–15, then 6
  and 0–15); 0 by default, which is the header as before. Neither FFmpeg
  nor `dca` applies it, so it changes no comparison. No dynamic range
  control.
- ✅ **`dts-encode`** takes `--ceiling` (bits a second, 24.5 Mb/s),
  `--buffer` (code, 8) and `--dialnorm` (dB, 0), prints the largest frame,
  the frames sent ahead and the buffer held, and `dts` fails the stream on
  a frame over the ceiling or a buffer starved or overfull.
- ✅ **Whole titles, bit for bit.** The four reference streams' 7.1 beds
  (24 bits) and 7.1.4, 34,311 frames each way, through `dca` (the 7.1.4:
  composite, heights and unfold) and FFmpeg (`-err_detect crccheck`), every
  header rewriting byte for byte and every navigation word found:

  | | 7.1, 24.5 Mb/s | 7.1, lowered | 7.1.4, 24.5 Mb/s | 7.1.4, lowered |
  |---|---|---|---|---|
  | stream 1 | 7.21 Mb/s at most | 7 Mb/s: 45 frames ahead, 8,280 B held | 10.94 | 10 Mb/s: 615 ahead, 34,080 B |
  | stream 2 | 6.05 | 6 Mb/s: 7 ahead, 6,066 B | 8.68 | 8 Mb/s: 103 ahead, 24,182 B |
  | stream 3 | 6.72 | 6 Mb/s: 206 ahead, 64,991 B | 9.95 | 9 Mb/s: 183 ahead, 82,614 B |
  | stream 4 | 7.12 | 6 Mb/s: 7,343 ahead, 96,694 B | 10.41 | 10 Mb/s: 71 ahead, 12,436 B |

  Every frame under its ceiling, to the byte (7,999 of 8,000 at 6 Mb/s),
  the buffer starved nowhere. Lower still, the domain answers instead:
  stream 1's 7.1 at 6 Mb/s and its 7.1.4 at 9 are refused at frames 1,935
  and 1,945 (the 1,025 substreams up to them 933 and 1,397 bytes short),
  stream 4's 7.1.4 at 9 at frame 15,706 (131,231 bytes held of 131,072).
- ✅ **A decoder joining mid-run.** Cut at substreams that start inside a
  frame, FFmpeg decodes the core alone for exactly the stated delay and
  then every frame bit for bit; the crate's tests check the same for every
  substream of a smoothed stream through `DecoderBuffer`.
- ✅ **The worst case.** Full-scale 24-bit white noise in all eight
  channels of a 7.1 is 10.9 Mb/s at its largest frame: under the default
  ceiling, whole frames, bit-exact. At 9 Mb/s, a quarter-second burst of it
  between tones is smoothed (37 frames ahead, 49 kB held) and bit-exact
  through both decoders; the noise alone is refused at frame 0. Ten
  seconds of it in two-second cycles with half-second bursts fit 9 Mb/s
  (300 frames ahead, 87 kB held) and are refused at 8 (frame 92, the
  buffer). In a 7.1.4, where the navigation word refuses louder noise
  (C3), bursts at 2⁻¹⁰ of full scale are smoothed under 8 and 7.5 Mb/s,
  bit-exact. `white_noise_bursts_are_smoothed_under_the_ceiling_or_refused`
  holds the 1-second cases; the crate's own tests hold the schedule, the
  joins, the refusals and the default's stating no buffer without either
  decoder.
- 🔴 **Not done**: an initial delay at the stream's start, which would let
  the first frames be spread too but has a decoder present them from the
  core meanwhile — not lossless; and a buffer stated on streams that never
  need it, which the feature-length streams do and the reference streams
  do not.

#### Phase C5 — Command line and containers

- `harlettizer dts` (or `encode --codec dts`) with the usual progress lines;
  raw `.dts` (core + EXSS) and Matroska (`A_DTS`), by generalising the IAMF
  Matroska writer.
- **Size**: about a week. **Risk**: low.

#### Phase C6 — Objects (research)

- The alternate profiles, which carry one to five objects beside the four
  heights. **Mode 1 only**: each object states its fold rows into the bed,
  so every decoder removes exactly what was added. The stated rows of the
  reference streams follow the point-source renderer of ETSI TS 103 584,
  quantised (it reproduces 3,994 of the 3,995 distinct positions observed),
  and a stated fold is exact integer arithmetic, `(x·Q15 + 2^14) >> 15`.
- **Mode 0 is never written.** In mode 0 the stream gives positions without
  rows, and the receiver re-derives each fold with its own renderer. No
  public document fixes that renderer, so a receiver whose law differs from
  ours would leave a ghost of every object in the bed, and no offline check
  would show it — to save a few bytes a frame.
- Clustering to the profile's object count through `hz-cluster`.
- **Accept when**: as C3, plus the objects at their positions on the
  receiver.
- **Size**: open-ended. **Risk**: high — the object grammar still has
  fields that are only pinned, and acceptance is a hardware question.

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
| The E-AC-3 Evolution frame's 32 + 8 protection bits: their computation is unknown, and no public decoder checks them | A receiver that checks them may refuse our objects while every offline check passes | Find out on hardware first — B2 re-encodes a reference stream's core and carries its payloads over, before any parameter is estimated. If a receiver refuses, only a trace of a **licensed** encoder can answer it (LEGAL.md §3.1) |
| Patent exposure | Legal, not technical | Publish the notice, take the posture of comparable projects |
| Scope drift into container/muxing/IMF work | Never finishes | Out of scope: use FFmpeg/mkvmerge for muxing. This project produces elementary streams and master files only |
| A measurement taken from an unlicensed run of the reference engine | Fatal, project-ending | Written rule in LEGAL.md §3.1; nothing enters the repository from one |
| DTS:X fields pinned to the values observed (the prefix wrapper, the navigation markers, the object grammar) | Every offline check passes and a licensed receiver refuses or ignores the extension | Play C3 on hardware before C6 is built on it; never vary a field that is not read |
| A DTS core ported from `dcaenc` is modest at 1.5 Mb/s | Legacy playback sounds worse than a reference stream's | The core is the compatibility layer, not the product; a better one is a separate job after C3 |

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
