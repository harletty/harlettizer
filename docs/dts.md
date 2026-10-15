# DTS:X output

```bash
harlettizer dts programme.atmos --out programme.mka            # DTS-HD MA + DTS:X, 7.1.4, in Matroska
harlettizer dts programme.atmos --out programme.dts            # the same, a raw elementary stream
harlettizer dts programme.atmos --out programme.dts --adpcm    # the core with subband prediction
harlettizer dts programme.atmos --out programme.mka --layout 7.1             # DTS-HD MA 7.1, 24-bit
harlettizer dts programme.atmos --out programme.mka --layout 5.1 --bits 16   # DTS-HD MA 5.1, 16-bit
```

Takes a master set or an ADM BW64 file at 48 kHz and writes DTS-HD Master
Audio with the DTS:X extension's **standard profile**, a fixed 7.1.4 — or,
with `--layout`, DTS-HD Master Audio 7.1 or 5.1 alone, at 24 or 16 bits. This
is phases C3 and C5 of Track C (see [PLAN.md](../PLAN.md)): the bitstream is
done and checked through two decoders; it has **not yet been played on a
receiver**, which is the one check that says a licensed decoder accepts it
(below).

## Layouts and containers

| `--layout` | What is coded | `--bits` |
|---|---|---|
| `7.1.4` (default) | the standard DTS:X profile, below | 24 |
| `7.1` | the 7.1 lossless beside the 5.1 core made from it, the 5.1 fold embedded, as phase C2 codes it | 24 or 16 |
| `5.1` | the 5.1 lossless beside a core of itself. The room's cube has no place for a surround at ±110°, so the master is rendered on 7.1 and folded as `ac3` folds it | 24 or 16 |

The output is a **raw elementary stream** — each frame its core, then its
extension substream — or, when `--out` ends in `.mka` or `.mkv`, the one track
of a **Matroska** file, `A_DTS`: the same frames, a block each. The track has
no private data (every frame states its own shape); `Channels` is what a
decoder of the lossless layer presents, eight for a 7.1.4, and `BitDepth` the
lossless layer's, as `mkvmerge` states them for the same stream. The last
block states the padding that fills the last frame as `DiscardPadding`, so a
player that applies it — FFmpeg does — hands out exactly the master's length;
the raw stream has nowhere to say it and decodes to whole frames. The muxer is
`hz-matroska`, the one IAMF's `.mka` uses.

`--progress` reports on standard error as every encode does.

## What is written

The master is rendered as `iamf` renders it — bed channels routed to their
speakers, objects panned on the room's cube — onto 7.1.4. Then, frame by
frame of 512 samples:

- **The fold.** Each height goes into the corner below it at −3 dB, `(h ·
  23170 + 2^14) >> 15` — TFL into L, TFR into R, TBL into the left rear
  surround, TBR into the right. The 7.1 that makes is the *composite*: what a
  7.1 decoder, or a DTS:X decoder that does not know the extension, plays.
- **The bed.** The composite, lossless at 24 bits, coded against a 5.1 core
  made from it, exactly as phase C2 codes a 7.1 (`hz_dca::lossless`,
  `hz_dca::master`).
- **The extension block**, at the end of the lossless frame
  (`hz_dca::dtsx`): the fold matrix that says what was folded where (22
  bytes, the same in every frame, written from its fields), and the four
  heights as a bare lossless channel set, every channel coded as itself.
- **The navigation word** in the asset descriptor, which says where the block
  starts and how long it is.

A DTS:X decoder takes each height back out of its corner, in integers, and
plays it above: the twelve channels come back bit for bit.

The substream header is the reference streams' in every field, down to the
padding that ends it on a 32-bit word.

## How it is checked

What `harlettizer dts` wrote, raw or `.mka`:

```bash
cargo run --release -p xtask --features dca-oracle -- dts programme.mka
ffprobe programme.mka              # dts, DTS-HD MA + DTS:X, 7.1
mkvmerge --identify programme.mka  # audio (DTS-HD Master Audio)
```

`xtask dts` reads an `.mka` through its `A_DTS` track: it rewrites every
header byte for byte, finds the navigation word on every 7.1.4 frame, and
decodes every frame through `dca`. On a 20-second synthetic master — eleven
objects over a 7.1.2 bed, `cargo xtask make-master --objects 11 --bed 7.1.2
--seconds 20` — each of the three layouts in both containers: the `.mka`'s
blocks are the `.dts` byte for byte (`mkvextract` agrees); every header
rewrites; `dca` decodes all 1,875 frames, presenting the height profile on
every 7.1.4 one; and the decode, re-encoded with `dts-encode` below — which
holds `dca` and FFmpeg to its input bit for bit — gives back the command's
stream byte for byte. `crates/hz-cli/tests/dts.rs` does the same on small masters at every
layout and depth, and, where FFmpeg is installed, holds its decode of the
`.mka` to the master: the 7.1 bit for bit, the 7.1.4's composite the master's
heights folded into its corners, exactly the master's length.

What any stream carries, from raw PCM:

```bash
cargo run -p xtask --features dca-oracle -- dts-encode in.s32 --channels 12 --lossless 24 --out out.dts
cargo run -p xtask --features dca-oracle -- dts stream.dts [--unfold out.s32]
```

`dts-encode` takes raw 32-bit 7.1.4 in WAV order (L R C LFE, the rears, the
sides, TFL TFR TBL TBR), encodes it, and then:

- rewrites every header byte for byte, the extension block's included;
- decodes it through `dca`, which has to present the standard height profile
  on every frame. The composite 7.1 it returns has to be the fold of the input,
  and the four heights the input's. The integer unfold of the two has to give
  the input's 7.1 back. All of it bit for bit;
- decodes it through FFmpeg, which has to report `DTS-HD MA + DTS:X` and
  return the composite 7.1 bit for bit, with its checksums checked.

`dts --unfold` writes any standard-profile stream's twelve channels before the
fold. That is how the material below was made: the four standard-profile
reference streams' own 7.1.4, which went through the same checks.

| Reference stream's 7.1.4 | frames | bed, mean / largest | heights, mean / largest | the reference encoder's bed | its heights |
|---|---:|---|---|---|---|
| 1 | 7,600 | 4,597 / 7,572 B | 2,881 / 4,996 B | 4,825 / 7,884 B | 2,896 / 5,056 B |
| 2 | 2,396 | 4,151 / 6,024 B | 2,815 / 3,628 B | 4,376 / 6,300 B | 2,824 / 3,636 B |
| 3 | 4,167 | 3,547 / 6,912 B | 2,391 / 4,376 B | 3,753 / 7,136 B | 2,457 / 4,432 B |
| 4 | 20,148 | 4,829 / 7,448 B | 3,118 / 4,540 B | 5,067 / 7,776 B | 3,150 / 4,524 B |

Every frame of the four: through `dca` the composite, the heights and their
unfold bit-exact, the height profile on every frame; through FFmpeg the 7.1
bit-exact and the DTS:X profile. The bed here is the navigated offset: the
bytes before the block.

## The limit: where the navigation word can point

The navigation word gives the block's offset in eleven bits of four bytes, so
**the bed cannot pass 8,188 bytes a frame** (about 6.1 Mb/s for the lossless
7.1), and the block cannot pass 8,212. No stream has been seen with a wider
field, so none is assumed. A frame that does not fit is not cut, nor coded
coarser: the encode stops with an error naming it.

```text
frame 0 (from sample 0): an extension block 9112 bytes into the frame cannot be
navigated (a multiple of four up to 8188), so the DTS:X extension cannot be
located; not cut to fit
```

The bed is the composite 7.1 less what the core already carries, so what
fills it is detail the core cannot predict:

- **Programme material fits.** The four reference 7.1.4s reach 7,572 bytes at
  most, and the reference encoder's own beds of the same material reach 7,884.
  An 11-object master, rendered, reached 7,716: the margin is real but not
  wide.
- **White noise does not.** Uniform white noise in all twelve channels is
  refused from a peak of 2⁻⁸ of full scale (−48 dBFS) upward, and encodes
  losslessly at 2⁻⁹ (−54 dBFS) and below.

## Headroom

A corner and its height together reach 1.707 of either's peak. A composite
past 24 bits would be clipped by a decoder, and the height could then not be
taken back out of it exactly, so the encoder refuses one
(`Refused::Fold`). `harlettizer dts` therefore limits before it quantises:
one gain for all twelve channels (`hz_cluster::mix::Limiter`, as an element's
mix has), watching the composite as well as the channels, holding all of it a
few steps under full scale. The summary says how loud the composite was before
the limiter and in how many blocks it acted. A synthetic 40-object scene
peaked at 3.11 and was held; an 11-object master peaked at 0.56 and the
limiter never acted.

## Fields pinned to what the streams carry

No published document describes the extension. These are written at the
value every frame of every standard-profile stream to hand carries, and their
meaning is not established (see LEGAL.md):

- in the fold matrix, every field but the masks, the level, the scale and the
  rows (`hz_dca::dtsx::FoldMatrix`);
- the byte after the matrix's checksum, `2`;
- the two zero bytes after the height set's data;
- the two zero bits before the navigation word in the descriptor's tail.

Our own decoders accept them by construction. Only a receiver can say whether
a licensed decoder does.

## Trying it on a receiver

Two checks only a person with the hardware can make. One is phase C3's
acceptance (c): it plays as DTS:X on a licensed receiver over HDMI, the
heights where they belong. The other is (d): listening.

1. Make a stream from a known-good one's content, so that the two can be
   compared on the same receiver:

   ```bash
   cargo run --release -p xtask --features dca-oracle -- dts reference.dts --unfold ref714.s32
   cargo run --release -p xtask --features dca-oracle -- dts-encode ref714.s32 --channels 12 --lossless 24 --out ours.dts
   ```

   and one from a master, in Matroska:

   ```bash
   harlettizer dts programme.atmos --out programme.mka
   ```

2. Play each through a player that passes DTS-HD through untouched. With mpv
   over HDMI, straight to the HDMI output so that nothing resamples or mixes
   it (`mpv --audio-device=help` lists the devices):

   ```bash
   mpv --ao=alsa --audio-device=alsa/hdmi:CARD=…,DEV=0 --audio-spdif=dts-hd --audio-channels=7.1 programme.mka
   ```

   `--audio-channels=7.1` lets mpv open the eight-channel 192 kHz link a
   DTS-HD stream needs; with less it falls back to the core. Any other player
   that bitstreams Matroska will do. Without a receiver, the same command
   with `--ao=pcm --ao-pcm-file=check.raw --ao-pcm-waveheader=no` writes what
   would go down the cable: one IEC 61937 DTS-HD burst (data type 0x11) per
   frame, at 192 kHz on eight channels.
3. The receiver should show **DTS:X**, not DTS-HD MA (which would mean it
   played the 7.1 alone, heights in the corners) and not DTS (the core alone).
4. Listen for the heights from the height speakers and nowhere else.
   `ours.dts` and `reference.dts` should sound the same: same content, same
   profile. A height still audible from a corner, or a corner thinner than in
   the reference, would mean the fold is not being undone. With the
   receiver's DTS:X processing off, the heights should come from the corners
   at −3 dB.

## Not done

- Peak-rate smoothing and dialnorm in the core header are phase C4. Every
  lossless frame sits whole in its own substream, so the substreams vary in
  size.
- The heights are always declared 24-bit. The reference encoder declares 22
  on some quiet frames.
- Objects: the alternate profiles, mode 1 only, are phase C6. It starts once
  this has played on hardware.
