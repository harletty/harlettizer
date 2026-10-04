# IAMF output

```bash
harlettizer iamf programme.atmos --out programme.iamf                # FLAC, lossless
harlettizer iamf programme.atmos --out programme.iamf --codec opus   # Opus, for distribution
harlettizer iamf programme.atmos --out programme.iamf --objects      # IAMF v2.0 objects
harlettizer iamf programme.atmos --out programme.mka                 # the same, in Matroska
```

Takes a master set or an ADM BW64 file, renders it to a 7.1.4 bed, and writes
the bed as an [IAMF](https://aomediacodec.github.io/iamf/) sequence — the
Alliance for Open Media's immersive audio format, the one YouTube and Chrome
call Eclipsa Audio.

## Why a bed, and why v1.1

IAMF had no objects until v2.0 (September 2026), and v2.0 is not what a browser
or a television decodes yet: Chrome, YouTube's players and libiamf before 2.0
read v1.1. In v1.1 an element is either channels — up to 7.1.4, or 9.1.6 in the
base-enhanced profile's expanded layouts — or a scene in ambisonics. A bed is
the one that plays everywhere, so by default the objects do not survive as
objects: they are rendered. `--objects` carries them as v2.0 objects instead —
see [Objects](#objects-iamf-v20).

What is written is the smallest sequence that carries one:

- **simple profile** — one audio element, at most sixteen channels;
- **one codec config** — FLAC (default) or LPCM, both lossless, or Opus;
- **one channel-based element**, `loudspeaker_layout` 7 (7.1.4), a single
  layer, no demixing or recon parameters — seven substreams, the front, side,
  rear, top-front and top-back pairs, then centre, then LFE (§3.6.3.3);
- **one mix presentation** playing that element at unity, stating the loudness
  on two layouts: stereo, which every sub-mix has to state (§3.7.4), and 7.1.4,
  which says what the mix was authored on.

## The render

Bed channels the layout has are routed to their speakers at unity. Objects —
and bed channels 7.1.4 lacks, a wide or a top side — are panned by
[`hz_render::Room`](../crates/hz-render/src/room.rs) and rendered by
[`hz_render::Mixdown`](../crates/hz-render/src/mixdown.rs), each update landing
on its own sample and its gains ramping over the samples it asks for.

The panning is on the **room's cube**, not by direction. A master's
`[-1, 1, 0]` is the front-left corner, where front left stands; read as a
direction it is 45°, and the hull panner the clustering metric uses puts it on
L three tenths of a decibel down with an image eleven decibels down behind the
listener (see the note in [`panner.rs`](../crates/hz-render/src/panner.rs)).
BS.2127's point conversion does not rescue it either: its map is the 0+5+0
one, which puts the rear corner at 110°, between a 7.1.4's side and rear. So
the room pans by the law [`fold.md`](fold.md) measured off a reference
stream's folds, taken up one more axis: speakers at their cube positions, an equal-power fade linear in the cube
coordinate between the two layers around the object's height, then the two
rows around its depth, then the two speakers around its width. Every speaker
position lands on its speaker alone and power is kept everywhere; the tests
in `room.rs` pin both.

What it does not honour, stated rather than approximated: an object's **size**
(the measured law has nowhere to put one) and its **rendering mode** —
snapping, zones, screen scaling — which are statements to a decoder's renderer,
and a bed has no renderer after it. An ambisonic or binaural track is left out
with a note.

The bed is rounded to the codec's integers — 24 bits unless asked — and
clamped; clipped samples are counted in the summary, and a warning says when
there are any.

## Loudness

A decoder normalises by the mix presentation's loudness, so it has to describe
what the decoder plays. Both figures are measured on the bed **after** it is
rounded, which is what a lossless decoder hands back — and what an Opus
decoder hands back to within what the codec changes, a tenth of a LU on the
programme below:

- **7.1.4** — BS.1770-4 through `hz-analysis`, the surrounds weighted, the LFE
  left out.
- **stereo** — the bed folded the decoder's way. A single-layer element has no
  demixing parameters, so the decoder renders it to stereo with its own
  direct-speakers renderer (§7.3.2.1.1); the matrix is the reference decoder's
  (libiamf `m2m_rdr.c`, as iamf-rs tabulates it): fronts and front heights to
  their side at unity, centre, sides, rears and rear heights at −3 dB, LFE
  dropped.

Each states the integrated loudness, the sampled peak and the true peak. The
descriptors come first in the file and the loudness is known last, so the
fields are written as placeholders and patched when the programme ends; they
are fixed-width, so nothing moves.

## FLAC as IAMF carries it

Each audio frame is one bare FLAC frame, every one exactly
`num_samples_per_frame` long (the last is padded and the padding trimmed in
the OBU header), and the codec config holds a `STREAMINFO` with both block
sizes the frame size, frame sizes and MD5 unknown and one channel (§3.11.3).
Two constraints narrow FLAC itself: the channel assignment is **0 or 1** — no
mid/side — and the frame header states its rate and depth outright.

The encoder ([`flac.rs`](../crates/hz-iamf/src/flac.rs)) is written from RFC
9639: per channel, the cheapest of a constant, verbatim, the five fixed
predictors, or a linear predictor (Tukey(½) window, Levinson-Durbin up to
order twelve, fifteen-bit coefficients with the rounding error carried), the
residual Rice-coded over the best of up to eight partition levels, with an
escape for raw partitions. It stays inside the streamable subset at 48 kHz.

**Depth is 16 or 24 bits.** RFC 9639 names 32-bit frames, but decoders older
than it — libFLAC before 1.4, symphonia — read that code as reserved and
refuse the frame. LPCM takes 32.

Measured on a 71-second programme rendered to 7.1.4, against `flac -8` at the
same block size:

| | bytes | |
|---|---|---|
| this encoder | 70 305 860 | 1.003 |
| `flac -8 --no-mid-side` | 70 095 353 | 1.000 |
| `flac -8` with mid/side, which IAMF forbids | 69 634 318 | 0.993 |
| LPCM | 122 559 309 | 1.748 |

About 1.5 s for the 71 s programme on one core, render included.

## Opus

```bash
cargo build --release -p hz-cli --features opus
harlettizer iamf programme.atmos --out programme.iamf --codec opus [--bitrate 64]
```

The one codec here that is not written here. A lossy coder is judged by ear
and by years of tuning, and libopus is the encoder the format was tuned with;
the pure-Rust ports on crates.io are young, and one that sounds worse is no
trade for a toolchain. So Opus is behind the `opus` feature, links the system's
libopus through `pkg-config`, and is the only `unsafe` in `hz-iamf` — five
functions of its C API in [`opus.rs`](../crates/hz-iamf/src/opus.rs). The
default build stays pure Rust.

**The released binaries have it**, with libopus linked statically — apt's
build on Linux, Homebrew's on macOS, vcpkg's on Windows — so nothing has to
be installed beside them; the release job checks the binary depends on no
libopus and encodes a few seconds of Opus with it before packaging, and
libopus's notice ships in `LICENSES/`. To build the same:

```bash
OPUS_LIB_DIR=$(pkg-config --variable=libdir opus) OPUS_STATIC=1 \
  cargo build --release -p hz-cli --features opus
```

Through `OPUS_LIB_DIR`, because the `pkg-config` crate will not link a
library in a system directory statically, whatever `OPUS_STATIC` says: it
links the shared one and says nothing — the first release build did exactly
that, and the `ldd` check caught it. Under `OPUS_LIB_DIR` a missing static
library fails the build instead (Arch ships none; Debian's and Ubuntu's
`libopus-dev` do).

What IAMF fixes (§3.11.1), and what is chosen:

- One packet per frame per substream, mono or stereo, at **48 kHz** — a
  programme at another rate is refused rather than resampled. 20 ms packets
  (960) unless asked; `audio_roll_distance` is −⌈3840 / frame⌉, −4 at 20 ms.
- The codec config is RFC 7845's identification header without its magic,
  big-endian, two channels, no output gain, mapping family 0 — the same
  eleven bytes YouTube's own IAMF streams carry.
- **The pre-skip is the encoder's lookahead** (312 samples), and every
  substream trims exactly that off its first frame. The programme's last
  samples leave the encoder that much late, so the sequence is fed that much
  more silence — in the last unit if it has room, in one more otherwise — and
  the overhang trimmed off the end. What a decoder keeps is the programme,
  sample for sample in length and in time.
- **Bitrate** is per channel, 64 kbit/s unless asked: a coupled pair at twice
  it, the LFE at a quarter and band-limited to narrowband, which is all it
  carries — about 720 kbit/s for a 7.1.4. YouTube's IAMF streams carry about
  46 a channel (sixteen mono substreams of third-order ambisonics, in a
  capture of one). Unconstrained VBR, complexity 10, music.

On the same 71 s programme: 6.5 MB at 732 kbit/s against 70 MB of FLAC, in
2.4 s. FFmpeg's native Opus decoder and libopus both decode every substream
to exactly the programme's length and aligned with the bed to the sample
(the LFE's cross-correlation peaks within ten samples, as close as a signal
under 120 Hz says); iamf-rs renders it, and FFmpeg's `ebur128` reads its
stereo at −23.2 LUFS and −2.0 dBTP against the stated −23.1 and −2.1. How it
*sounds* is not something any of that checks.

## Objects (IAMF v2.0)

```bash
harlettizer iamf programme.atmos --out programme.iamf --objects [--positions cart16|cart8|polar] [--elements N]
harlettizer iamf vf.atmos --out vf.iamf --objects --overlay 7          # a re-voiced programme, encode --overlay's way
harlettizer iamf vf.atmos --out vf.iamf --objects --voices-to-bed 7    # the voices as a dialogue element
```

Each object of the master becomes an audio element of its own: one mono
substream, positioned by a parameter its mix presentation declares in the
rendering config — where v2.0 put it so that a v1.1 parser steps over it —
and animated by parameter blocks. Beside the objects, the master's **bed**
travels as one channel-based element (see [The bed element](#the-bed-element)).

An IA sequence carries at most twenty-eight channels. The header says which
profile the sequence needs: base-advanced for objects alone, advanced-1 up to
eighteen elements and channels, advanced-2 up to twenty-eight.

### The bed element

The master's bed channels — the LFE among them — become **one** channel-based
element, on the smallest IAMF loudspeaker layout that has every one of them:

1. the LFE alone is expanded layout 0, the LFE subset of 7.1.4, one substream
   — what a master decoded from a delivery stream brings, since that is the
   only bed a decoder can reconstruct;
2. anything else takes the layout with the fewest channels among IAMF's
   single-layer loudspeaker layouts 0 to 8 — mono, stereo, 5.1, 3.1.2, 5.1.2,
   7.1, 5.1.4, 7.1.2, 7.1.4, the lower code first between two of one width —
   that has them all; a channel of the layout the master lacks is silent;
3. a bed channel none of those layouts has — a top side (`Lts`/`Rts`, which
   IAMF's 7.1.2 does not have: its pair is top *front*), a wide, a second LFE
   — stays an object that stands at its speaker's place and never moves,
   which costs no parameter block at all: its definition's default is where
   it is. So an authored 7.1.2 bed is a 7.1 element and two still objects.

Channels are matched by what they are, not by how they are spelt: a 5.1
surround at `M+110` is the side surround a 7.1 calls `M+090`, and 5.1.4's top
back pair at `U±110` is the top rear pair a 7.1.4 has at `U±135` (one alias
each in `hz_core::speakers`). IAMF also has expanded layouts for side, rear
and height pairs, 3.0 and 9.1.6 — the last of which has the top sides and the
wides — and they are **not** used, because no decoder to hand renders them:
iamf-rs takes layouts 0 to 8 and, in the fork, the expanded LFE. Every
layout's substream order is the reference decoder's decoding map, and its
stereo row the reference decoder's down-mix (libiamf `m2m_rdr.c` as iamf-rs
tabulates it); both are pinned in `hz_iamf::layout`'s tests.

A master whose bed is the LFE alone writes the sequence it wrote before there
was a bed element, byte for byte.

### `--overlay`: a re-voiced programme, kept

`--overlay K` is [`encode --overlay`](encode.md#--overlay-keeping-a-scene-and-adding-to-it),
written as IAMF: the master's last K objects are sources — a dubbed dialogue,
one object per channel of the track it came from — and everything before them
is an element and is **kept**: its bed as the bed element, its objects as
object elements on the master's own paths. The sources are panned onto the
elements by `hz_cluster::overlay`, block by block, and an element no source
reaches is copied, neither mixed nor rounded.

It is the same computation and not a second one. The overlay's arithmetic —
the carriers, the beds-first fit, the fallback, every guard, which elements
are copied and which mixed, the ramps, the limiter, the account
`--overlay-report` writes and the verdict — lives in one module,
`crates/hz-programme/src/overlay.rs`, and both writers call it: on the same
1280-sample blocks (26.7 ms at 48 kHz), with the elements numbered the same
way (the LFE first, then every other track in the master's order) and each
block placed on the same state — every element at the update in force at the
block's end. What the two writers state differently is only what a decoder
does with an element's gain. A TrueHD element's gain is metadata its decoder
applies, in whole decibels, so a source added to it is divided by it first;
an IAMF object has no gain but its mix gain, so the master's gain is in its
samples, ramps included, as for any object here, and a source is added as it
is. An element whose gain is under −100 dB is silenced in both and is never a
carrier.

The options are `encode`'s, with its names and its defaults, from one
definition shared by both commands: the guards `--overlay-drift`,
`--overlay-spread`, `--overlay-wobble`, `--overlay-level`, `--overlay-cost`,
`--overlay-fallback`, the preference `--overlay-beds`, `--overlay-spare`,
`--overlay-report`, and the four that shape what a mix makes —
`--fold-depth` (the mixed elements' depth, 20 bits unless asked; an IA
sequence takes 16 to 24 and never more than its `--bits`, and FLAC takes the
zeroes off the bottom of a subframe as wasted bits), `--dialnorm` (what
counts as audible, and so what is mixed rather than copied), `--headroom` (its
limiter half) and `--loudness` (the weighing that decides audibility). They
change the elements, so **they must match `encode`'s for the two to write the
same overlay** — `--fold-depth` most of all, since it decides every mixed
element's low bits. What `encode` has and an overlay does not use is not here:
`--fast`, `--presentations` and `--drc` are the TrueHD stream's, and
`--cluster`, `--beds`, `--fold-search`, `--fold-hold` and `--smooth-behind`
are the clustering's.

The element budget is the master's own: the sequence carries exactly the
elements the master brought — as many as IAMF's twenty-eight channels hold, the
bed element's counted — and with `--overlay-spare` a source may take one of
what is left of them. An overlay folds nothing, so a master wider than that is
refused (`--objects` alone folds it). Refusals are `encode`'s, word for word:
`error: overlay: …` on standard error, a non-zero exit, and the stream left
on disk.

**Checked against TrueHD, as an oracle only.** On the first five minutes of a
real AtmosFer re-voiced master (Tron: Ares, LFE + 20 objects, the last 7 the
French voices; the options its TrueHD encode was made with less the TrueHD
ones: `--overlay 7 --fold-depth 20 --overlay-drift 0 --overlay-cost 0`):

| | |
|---|---|
| the `--overlay-report` account, every block's copies, mixes, weights and gains | byte-identical to `encode`'s |
| the 14 elements as decoded — `harletty decode` of the TrueHD stream, presentation 3, against the IAMF fork's decode of the FLAC sequence | all 14 bit-exact, 14 400 000 samples each |
| the 13 object elements' positions, every 256 samples, 731 250 of them | within half an LSB of `cart16` of the master's, every jump on its sample |
| a synthetic master with a declared 7.1.2 bed and 8 objects, `--overlay 3` | report byte-identical; all 15 elements bit-exact against the TrueHD decode — 8 of them in the 7.1 bed element, sources mixed into its channels |

Bit-exact because every element of those masters is at unity gain: where an
element states another, the TrueHD stream carries its samples and the gain
apart and the IAMF sequence carries their product, so the two agree as
rendered and not sample for sample.

### `--voices-to-bed`: a dialogue element

`--voices-to-bed K` gives the master's last K objects — a dub's voices — an
element of their own. The rest is the M&E and is carried exactly as
`--objects` carries a master: its objects as object elements (folded only past
what IAMF carries), its bed as its bed element — an LFE alone stays an LFE
element. The voices are rendered on the room's cube — `hz_render::Room`, the
panner the bed mode renders with: each voice panned where its updates put it,
its gains ramping over the samples each asks for — into **one channel-based
element**, the last of the sequence.

**Its layout** is the smallest of the loudspeaker layouts above that *holds*
every place the voices go: every place their updates put them and, between
two updates in a row, eight points along the way. A layout holds a place when
every speaker the room pans it onto on 7.1.4 — what this workspace renders and
measures on — is one of its own, under the same label; then the voices
rendered into it and played on 7.1.4 are the voices rendered on 7.1.4, and the
smaller layout loses nothing. A voice at the centre is mono; the front three
are 5.1 (IAMF's 3.0 would hold them in three channels, and is an expanded
layout no decoder to hand renders); the seven places of a 7.1 are 7.1 — not
5.1, whose surrounds stand at ±110°, between 7.1.4's; a front height is 3.1.2,
or 7.1.2 beside the side surrounds; anything else is 7.1.4. A dub's voices are
the channels of the track they came from, placed once at the original's
speaker places, so the rule is decided by a handful of points, and a voice at
a speaker's place lands on that channel alone — its samples, unchanged.

**The mix presentation says what it is.** With a dialogue element:

- **labels**, one language (`en-us`) for the mix (`Main`) and every element
  — `M&E` for the bed and the objects, `Dialogue` for the voices — since a mix
  presentation states as many labels for each element as for itself;
- an **element gain offset** on the dialogue (IAMF v2.0, in the rendering
  config's extension, flagged in a bit a v1.1 parser reads as reserved): the
  range type, 0 dB by default and −12 to +12 dB for a player to let a listener
  move it. libiamf's vector `test_000854` (−3 dB within −6 and 0) reads the
  bounds as absolute; the iamf-rs fork's parser comments them as relative to
  the default. With a default of 0 dB the two readings agree;
- a **loudness anchored on dialogue** on both layouts (`info_type` bit 1,
  `anchor_element` 1 — the byte libiamf's vector `test_000062` carries for its
  `ANCHOR_TYPE_DIALOGUE`, which iamf-tools' enumeration numbers 2): the
  BS.1770 integrated loudness of the dialogue element alone, rendered and
  folded exactly as the mix is measured, gated as BS.1770 gates and with no
  speech gate — the element carries nothing but the voices.

Checked on the first five minutes of Tron: Ares (`--voices-to-bed 7`): the
voices at the seven places of a 7.1 make a 7.1 dialogue element — beside the
LFE element and 13 object elements, 15 elements and 22 channels, advanced-2.
Decoded by the IAMF fork, the dialogue's channels are the trace's bit for bit
and each the master's voice of that place, unchanged; the 13 objects and the
LFE are the master's; and the fork's own descriptor parser reads back the
labels, the range (0, −3072, +3072 in Q7.8) and the anchored loudness
(−24.2 LKFS on 7.1.4, −23.8 on stereo, against the mix's −17.1 and −16.5). On a
synthetic 7.1.2 bed with voices moving overhead, the dialogue is a 7.1.4
element, and the decoder's System J bed is the bed element plus the dialogue
element to the sample. FFmpeg 9 lists the labels; it stops with `Invalid data`
on any sequence of two elements, labels or not, so that says nothing about
these.

The flag keeps its name, from when the voices went into the bed; front ends
pass it as before. The summary's `voices` line is now a `dialogue` line, and an
`anchored` line follows the loudness.

### Positions
### Positions

A master moves an object by updates — be here, this loud, take this long — so
its path through the room is piecewise linear, and IAMF's position blocks are
exactly that: runs of subblocks, each a step or a line. The path is computed
once from the updates (a ramp an update cuts short is left from where it had
got to), and every block is cut from it: a unit's subblocks break where the
path does, so a move that starts mid-unit starts on its sample; an object
standing still costs one block for as long as it stands — up to ten seconds,
restated after that (see below); and the codec's
delay is carried, the blocks counting on the sequence's timeline while the
audio is trimmed by the pre-skip. Coded as the master's cube coordinates —
`cart16` (default) or `cart8` — or as `polar`, the cube put on the sphere by
BS.2127's conversion. On a 71 s programme of eleven objects, positions cost
about 7 kbit/s in `cart16`.

**Ten seconds a block at most.** libiamf scales a parameter's durations to
the sample clock by `(rate + 0.1) / parameter_rate` and truncates, and iamf-rs
does as it does, so a block of `d` ticks lasts `⌊d · (1 + 0.1 / rate)⌋`
samples: exactly `d` below ten seconds' worth, one sample more from there —
and every block after it starts that much late. An object standing still for
minutes as one block had its next move decoded up to tens of samples late, and
the decoder's positions, sampled every 256, put a jump a whole step late
(found on Tron, after a 256-unit standstill). A standstill is therefore
restated every ten seconds — 117 units of 4096 at 48 kHz — for about 2 % more
blocks.

An object element has a mix gain and nothing finer, so an object's **gain**
— ramps included — is applied to its samples, sample by sample. Its **size**
and **rendering mode** (snap, zones, screen) have nowhere to go and are
dropped.

### More objects than elements

When the master's objects (and the bed channels no layout has) outnumber the
elements — what is left of twenty-eight channels beside the bed element unless
`--elements` says fewer; seventeen beside an LFE element keeps a sequence
within advanced-1 — the objects are folded into the elements block
by block with `hz-cluster`, the fold `encode --cluster` makes: where each
element goes, and how much of each object it carries, the weights ramping
across each block and each element's position ramping with them to where the
block put it. Bed channels outside the bed element keep elements of their
own. Blocks are the largest
divisor of the unit no longer than 1280 samples (1024 for a 4096 unit, 960 for
Opus), so that one never straddles two units.

It is placed as `encode --cluster` places it, by the same code
(`hz_programme::fold::Placer`): each block steered by the energies of the
window around it — `hz_cluster::smooth::SMOOTHING`, two blocks ahead, so that
an element is where a sound arrives when it does (see `docs/clustering.md`).
So the fold reads its own blocks two ahead of what it folds and hands the
folded frames out a unit at a time; a block is read whole whatever `--frames`
says, as an overlay's is. The mix takes the command's `--dialnorm` (the floor
under what an object has to carry to be heard), `--loudness` (how a block's
power is weighed), `--headroom` (the fit's bound on an element's coherent
peak, the limiter, both or neither) and `--fold-depth` (the mixed elements
rounded to it, 16 to 24 and never finer than `--bits`: twenty unless asked,
whose zeroed low bits FLAC drops as wasted bits). The summary reports what the
fold cost on the metric of `docs/clustering.md`, the floor, the bound and the
limiter. Synthetic 40-object scenes over a 7.1.2 bed (`cargo xtask
make-master --objects 40 --bed 7.1.2`, ten seconds) into 17 elements, FLAC,
before the look-ahead and the options reached this fold and after:

| scene | mean / worst, before | mean / worst, after | bytes, before → after |
|---|---|---|---|
| `tones` | 0.0289 / 0.260 | 0.0289 / 0.260 | 16.6 M → 11.6 M |
| `broadband` | 0.0175 / 0.431 | 0.0176 / 0.435 | 23.9 M → 19.8 M |
| `bursts` | 0.0099 / 0.580 | 0.0104 / 0.664 | 22.6 M → 18.7 M |

The per-block metric does not see what the look-ahead is for — an element
arriving with the sound rather than a block after it, and fewer flips — and
on the scene with onsets it pays a few per cent of it, as `docs/clustering.md`
measured at sixteen elements; the bytes are the twenty-bit rounding.

### Loudness

The mix presentation states the loudness on stereo and 7.1.4, measured on what
a decoder is handed rendered the way the bed mode renders — every object on the
room's cube along the path the blocks give it, the LFE to its speaker. How a
v2.0 decoder renders objects is the Open Audio Renderer's business, not the
stream's, so this is the measurement of one rendering, not of every one.

### Checked against

Nothing decodes IAMF v2.0 objects here except the iamf-rs fork's object
passthrough (`harletty/iamf-rs`, the v2.0 parser and position animation),
which hands out each object's samples and its position every 256 samples.
It had no expanded layouts, so the LFE element was checked with a local
patch to it — expanded layout 0 rendered as 7.1.4 with every other channel
empty, as IAMF §7.3.2.1 says. `HZ_IAMF_TRACE=<dir>` makes the encode write what
it meant — every element's samples, every object's position at the same
offsets — and the decoder's output is compared with it:

| | |
|---|---|
| LPCM and FLAC, the real programme (11 objects + LFE) | every object's samples exact, to the length; the LFE exact in the 7.1.4 render and nothing else in it; 20 625 positions to half an LSB of `cart16` |
| Opus, the same | every object to the programme's length and aligned to the sample; 22 011 positions on the master's timeline, the pre-skip accounted, to 1e-5 |
| 40 objects + 7.1.2 bed folded into 27 + LFE (advanced-2, substream ids past 17 stated in the frames) | all 27 elements exact; LFE exact; 101 250 positions to half an LSB |
| advanced-1 (`--elements 17`) Opus `cart8`, and `polar` FLAC | decoded |
| `harletty --codec iamf decode`, which writes a master set: Tron, LFE + 20 objects, 5 min, FLAC, `--objects` / `--voices-to-bed 7` | all 21 channels bit-exact against the master; with the voices in a 7.1.4 bed, its 12 channels exact against the trace and the 13 objects and the LFE against the master |
| a synthetic 7.1.2 bed + 8 objects, LPCM | the 7.1 bed element and the two top sides as still objects, all 18 channels bit-exact against the master |

The syntax — parameter definition types, coordinate widths, the animation
codes and their bit packing, `ObjectsConfig` — was read from libiamf's v2.0
test vectors and their iamf-tools descriptions and from the fork's parser;
`hz_iamf::position` reproduces a vector's bytes in its tests. The IAMF v2.0
text itself was not to hand.

Found on the way, and fixed: every object began silent and faded in over its
first update's ramp — 1536 samples on every master seen — because a path
started at gain nought. The trace said the same thing the stream did, so the
checks above did not see it; a decode against the master did (171 samples of
near-silence lost on a re-voiced master, 2461 on seqA). An object is now where
and as loud as its first update says from sample nought, which is what
`encode` does.

**Not checked**: libiamf 2.0 or any player. A v1.1 reader refuses the
sequence: FFmpeg 9 skips the object elements as a type it does not know, then
stops at the mix presentation that names them (`Invalid Audio Element with id
2 referenced by Mix Parameters 2`), and a decoder that honours the header's
profile stops sooner.

## Checked against

Nothing here is checked against itself:

- **FFmpeg** (9.0) demuxes the sequence without a warning, reads both
  loudness blocks back to the value, and decodes all seven FLAC substreams to
  exactly the samples the LPCM encode of the same programme carries — and to
  exactly the programme's length, so the trim is honoured.
- **libFLAC** (1.5) `flac -t` passes every frame, CRC-8 and CRC-16 included.
- **iamf-rs** (`iamfdec`) renders the FLAC and LPCM sequences to identical
  output. Its 7.1.4 render matches the bed channel for channel to half a
  16-bit LSB (its output is 16-bit), so the substream order is right, and its
  stereo render matches the fold the stereo loudness was measured through to
  the same half LSB. FFmpeg's `ebur128` on that stereo render reads the
  integrated loudness and true peak the stream states.
- **In the test suite**, symphonia decodes FLAC frames of every depth and
  shape back to the samples given, and the sequence's OBU layout, trims and
  patched loudness are read back field by field.

Not checked: a browser. Chrome decodes IAMF from 153 and the one to hand was
152; and whether Chromium's IAMF path takes FLAC substreams at all, rather
than only Opus and AAC, is untested. The Opus sequence is the one to try
first.

Found on the way: the pure-Rust `opus-decoder` crate (0.1.1), which iamf-rs
uses when it is built without libopus, overflows a shift building its
anti-collapse mask (`1u8 << i` with `i` past seven) on packets libopus writes
for an ordinary tone — a panic in a debug build and the wrong mask bit in a
release one. The tests here decode with libopus instead.

## Into Matroska

```bash
harlettizer iamf programme.atmos --out programme.mka [any other option]
```

An `--out` ending in `.mka` or `.mkv` puts the sequence in Matroska, as the
file's one audio track — the sequence itself is the same, whatever the other
options ask for. Matroska has no IAMF mapping yet
([matroska-specification#940](https://github.com/ietf-wg-cellar/matroska-specification/issues/940)),
so the file follows the draft Omniphony proposed there, which reads IAMF's
own ISO-BMFF encapsulation into Matroska the way the AV1 mapping does:

- `CodecID` `A_IAMF`, and a `CodecPrivate` that is the `IAConfigurationBox`
  payload — the version byte 1, the descriptors' length as a LEB128, then the
  descriptors, loudness patched in as in the standalone stream;
- one temporal unit per `SimpleBlock`, its OBUs exactly as the standalone
  stream has them but for the temporal delimiter, which a block makes
  redundant — every unit is a key frame;
- the trims stay in the OBUs, the decoder's to apply: `CodecDelay` states the
  start trim (Opus's pre-skip, nothing for a lossless codec) and the segment's
  `Duration` what is left after both, and no `DiscardPadding` asks for the end
  trim twice;
- `SeekPreRoll` from the roll distance — 80 ms for Opus at 20 ms frames,
  nought otherwise;
- `Channels` 2, which a reader ignores, as IAMF's own codec headers do;
  `BitDepth` for FLAC and LPCM; a cue on every cluster, every five seconds.

It is written as it goes, a cluster at a time, like the standalone stream:
the sizes, the duration and the seek head are fixed-width placeholders
patched at the end, so the output has to be a file, not a pipe.

Checked: the codec private data and the blocks, a delimiter put back before
each, are the standalone stream byte for byte (in the tests, and on the
71 s programme as FLAC, Opus and v2.0 objects); `mkvinfo` and `mkvmerge -J`
read the files without an error or a warning; and mkvmerge 101 carries the
track through a remux untouched — codec private data, `CodecDelay`,
`SeekPreRoll`, key frames — with `--sync`, a name, a language and tags added,
which is how a track joins a film. FFmpeg 9 does not know `A_IAMF`; the
mapping's reference reader is Omniphony's mpv fork.

## Into MP4

The output is a standalone OBU stream. FFmpeg puts it into an MP4 with one
IAMF track (`iamf` sample entry, `iacb`, an edit list for the trim) — but only
if it is told to carry the two stream groups and to keep the substream ids:

```bash
ffmpeg -i programme.iamf -map 0 -c:a copy \
  -stream_group map=0=0:st=0:st=1:st=2:st=3:st=4:st=5:st=6 \
  -stream_group map=0=1:stg=0 \
  -streamid 0:0 -streamid 1:1 -streamid 2:2 -streamid 3:3 \
  -streamid 4:4 -streamid 5:5 -streamid 6:6 \
  programme.mp4
```

Without the stream groups it writes seven unrelated tracks, which no IAMF
player will recognise. For Opus the MP4 carries the `roll` sample groups the
encapsulation requires (§6.2.2), from the codec config's roll distance.

## Options

| Option | Default | What it does |
|---|---|---|
| `--out <FILE>` | required | Write the sequence here: in Matroska if it ends in `.mka` or `.mkv`, a standalone stream otherwise |
| `--codec <CODEC>` | `flac` | `flac`, `lpcm`, or `opus` in a build with the `opus` feature |
| `--bits <BITS>` | `24` | 16 or 24; 32 for LPCM; none for Opus |
| `--bitrate <KBPS>` | `64` | Opus only: kilobits a second for each channel |
| `--frame-size <SAMPLES>` | `4096`, Opus `960` | Samples a channel per temporal unit: the FLAC block size, at most 4608; an Opus packet's length, 480, 960, 1920 or 2880 |
| `--headphones <MODE>` | `stereo` | What a decoder playing to headphones does: `stereo`, the loudspeaker fold, or `binaural`, its own binaural renderer |
| `--frames <N>` | | Stop after this many frames of audio |
| `--mono-prefix <PREFIX>` | | As for `encode` |
| `--objects` | off | Carry the objects as IAMF v2.0 objects rather than rendering a bed |
| `--positions <CODING>` | `cart16` | With `--objects`: `cart16`, `cart8` or `polar` |
| `--elements <N>` | what the bed element leaves of 28 | With `--objects`: the most object elements; more objects than that are folded into them |
| `--overlay <K>` | | With `--objects`: keep the master's elements and pan its last K objects onto them, as `encode --overlay` |
| `--voices-to-bed <K>` | | With `--objects`: render the last K objects into a dialogue element of their own — labelled, ±12 dB for a listener, the loudness anchored on it — and carry the rest as `--objects` does |
| `--overlay-beds`, `--overlay-drift`, `--overlay-spread`, `--overlay-wobble`, `--overlay-level`, `--overlay-cost`, `--overlay-fallback`, `--overlay-spare`, `--overlay-report` | as `encode` | With `--overlay`: as for `encode` |
| `--fold-depth`, `--dialnorm`, `--headroom`, `--loudness` | `20`, `-31`, `limit`, `flat` | With `--overlay`, or `--objects` folding: as for `encode`; the depth 16 to 24 |
| `--progress` | off | As for `encode` |

## Not done

- **AAC.** Its free encoder, FDK, is not GPL-compatible.
- **A v1.1 fallback** beside the objects — a second mix presentation over a
  rendered bed, for decoders that do not read v2.0 — which would cost the bed's
  channels on top of the objects'.
- **The expanded layouts** other than the LFE — 9.1.6 would hold an authored
  7.1.2 bed's top sides and a 9.1.6 bed's wides in the bed element — until a
  decoder renders them.
- **Two objects to a substream**, which v2.0 allows and Opus would code
  jointly.
- **Scalable layers** (a stereo or 5.1 core with 7.1.4 on top), which need
  demixing parameters and, for lossy codecs, recon gains.
- **Anchored loudness** (dialogue), which `hz-analysis`'s speech gate could
  give.
- **An MP4 writer** of its own.
