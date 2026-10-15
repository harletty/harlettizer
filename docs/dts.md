# DTS:X output

```bash
harlettizer dts programme.atmos --out programme.dts            # DTS-HD MA + DTS:X, 7.1.4
harlettizer dts programme.atmos --out programme.dts --adpcm    # the core with subband prediction
```

Takes a master set or an ADM BW64 file at 48 kHz and writes DTS-HD Master
Audio with the DTS:X extension's **standard profile**, a fixed 7.1.4, as a raw
elementary stream. This is phase C3 of Track C (see [PLAN.md](../PLAN.md)):
the bitstream is done and checked through two decoders; it has **not yet been
played on a receiver**, which is the one check that says a licensed decoder
accepts it (below).

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

## Peak bit rate

The stream is held under a ceiling, the core and every header included:
24.5 Mb/s by default (`hz_dca::peak::Config`). A lossless frame goes whole in
its own substream, as the reference streams have every one. Only a frame too
large for its own substream is sent partly ahead, in the substreams just
before it, into the decoder's smoothing buffer — as late as it can be, the
schedule measured on the feature-length streams that smooth — and the stream
then states that buffer: 128 kB by default, as they do. At 24.5 Mb/s no frame
the encoder can write is that large (a 7.1.4 is bounded by the navigation
word, 16,400 bytes, against about 30,600 of room), so the stream states no
buffer and every frame goes out as it is encoded. A frame that cannot reach
the decoder in time even so is refused with `Refused::PeakRate`, named, and
never coded coarser. `dts-encode --ceiling`, `--buffer` and `--dialnorm` set
the ceiling, the buffer and the core header's dialogue normalisation; `cargo
xtask dts` feeds every substream to the buffer as FFmpeg and `dca` keep it, and
reports the rate and what the buffer held (`--rate-log` frame by frame).

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

   and one from a master:

   ```bash
   harlettizer dts programme.atmos --out programme.dts
   ```

2. Play each through a player that passes DTS-HD through untouched, for
   example `mpv --audio-spdif=dts-hd --audio-channels=7.1 ours.dts`, or after
   `mkvmerge -o ours.mkv ours.dts` from any player that bitstreams.
3. The receiver should show **DTS:X**, not DTS-HD MA (which would mean it
   played the 7.1 alone, heights in the corners) and not DTS (the core alone).
4. Listen for the heights from the height speakers and nowhere else.
   `ours.dts` and `reference.dts` should sound the same: same content, same
   profile. A height still audible from a corner, or a corner thinner than in
   the reference, would mean the fold is not being undone. With the
   receiver's DTS:X processing off, the heights should come from the corners
   at −3 dB.

## Not done

- Matroska output and the usual progress lines are phase C5. Until then,
  `mkvmerge` wraps the raw stream.
- Peak-rate smoothing and dialnorm in the core header are phase C4. Every
  lossless frame sits whole in its own substream, so the substreams vary in
  size.
- The heights are always declared 24-bit. The reference encoder declares 22
  on some quiet frames.
- Objects: the alternate profiles, mode 1 only, are phase C6. It starts once
  this has played on hardware.
