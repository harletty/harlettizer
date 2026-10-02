# IAMF output

```bash
harlettizer iamf programme.atmos --out programme.iamf                # FLAC, lossless
harlettizer iamf programme.atmos --out programme.iamf --codec opus   # Opus, for distribution
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
the one that plays everywhere, so the objects do not survive as objects: they
are rendered.

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
| `--out <FILE>` | required | Write the sequence here |
| `--codec <CODEC>` | `flac` | `flac`, `lpcm`, or `opus` in a build with the `opus` feature |
| `--bits <BITS>` | `24` | 16 or 24; 32 for LPCM; none for Opus |
| `--bitrate <KBPS>` | `64` | Opus only: kilobits a second for each channel |
| `--frame-size <SAMPLES>` | `4096`, Opus `960` | Samples a channel per temporal unit: the FLAC block size, at most 4608; an Opus packet's length, 480, 960, 1920 or 2880 |
| `--headphones <MODE>` | `stereo` | What a decoder playing to headphones does: `stereo`, the loudspeaker fold, or `binaural`, its own binaural renderer |
| `--frames <N>` | | Stop after this many frames of audio |
| `--mono-prefix <PREFIX>` | | As for `encode` |
| `--progress` | off | As for `encode` |

## Not done

- **AAC.** Its free encoder, FDK, is not GPL-compatible.
- **Objects.** IAMF v2.0's object elements would carry the master's objects
  through `hz-cluster` to at most 18 or 28 channels; the encode pipeline would
  have to be separated from MLP first.
- **Scalable layers** (a stereo or 5.1 core with 7.1.4 on top), which need
  demixing parameters and, for lossy codecs, recon gains.
- **Anchored loudness** (dialogue), which `hz-analysis`'s speech gate could
  give.
- **An MP4 writer** of its own.
