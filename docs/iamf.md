# IAMF output

```bash
harlettizer iamf programme.atmos --out programme.iamf
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
- **one codec config** — FLAC (default) or LPCM, both lossless;
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
rounded, which is what a decoder hands back:

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
than only Opus and AAC, is untested.

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

Without the stream groups it writes seven unrelated FLAC tracks, which no IAMF
player will recognise.

## Options

| Option | Default | What it does |
|---|---|---|
| `--out <FILE>` | required | Write the sequence here |
| `--codec <CODEC>` | `flac` | `flac` or `lpcm` |
| `--bits <BITS>` | `24` | 16 or 24; 32 for LPCM |
| `--frame-size <SAMPLES>` | `4096` | Samples a channel per temporal unit, and so the FLAC block size; at most 4608 for FLAC |
| `--headphones <MODE>` | `stereo` | What a decoder playing to headphones does: `stereo`, the loudspeaker fold, or `binaural`, its own binaural renderer |
| `--frames <N>` | | Stop after this many frames of audio |
| `--mono-prefix <PREFIX>` | | As for `encode` |
| `--progress` | off | As for `encode` |

## Not done

- **Lossy codecs.** Distribution — YouTube, a browser — is Opus or AAC. Opus
  means libopus behind a cargo feature: there is no pure-Rust Opus encoder
  worth shipping, and AAC's free encoder is not GPL-compatible.
- **Objects.** IAMF v2.0's object elements would carry the master's objects
  through `hz-cluster` to at most 18 or 28 channels; the encode pipeline would
  have to be separated from MLP first.
- **Scalable layers** (a stereo or 5.1 core with 7.1.4 on top), which need
  demixing parameters and, for lossy codecs, recon gains.
- **Anchored loudness** (dialogue), which `hz-analysis`'s speech gate could
  give.
- **An MP4 writer** of its own.
