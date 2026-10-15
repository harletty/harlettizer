# E-AC-3 output

```bash
harlettizer ac3 programme.atmos --out programme.ec3 --eac3 --bitrate 768   # 5.1, one independent substream
```

Phase B2 of the lossy track (see [PLAN.md](../PLAN.md)), the part that joint
object coding needs: one independent E-AC-3 substream, 5.1 or anything else
AC-3 carries, at any rate a frame of whole 16-bit words can say — up to
1024 kb/s at 48 kHz — with room in block 2 for the Evolution frame that
carries a stream's objects. The render is `ac3`'s ([docs/ac3.md](ac3.md)).

## The encoder

The same as AC-3's — `hz-ac3` with `Format::Eac3` — and the same port of
FFmpeg (`libavcodec/eac3enc.c` and the E-AC-3 branches of `ac3enc.c`), so the
same transform, coupling, rematrixing, exponents and allocation, in E-AC-3's
syntax:

- the bitstream information of an independent substream, `bsid` 16, six blocks
  a frame, the frame size in words and alternating to keep any rate;
- the audio frame header: which tools the blocks use — none of block switching,
  dither flags, delta allocation or spectral extension — the coupling strategy
  once, and the exponent strategies as one five-bit code a channel wherever its
  six strategies are one of the thirty-two the code names (nearly always);
- one SNR offset for the frame; one CRC, over the whole frame.

FFmpeg's count of the frame's fixed bits assumes per-block exponent strategies,
so a frame written with the frame-level codes has a few spare bits: kept, so
that the allocation is FFmpeg's.

## Where the objects go

Every E-AC-3 stream with objects to hand carries its Evolution frame — object
metadata, joint object coding, programme loudness and information — in the
**skip field of block 2**, one per syncframe, and nothing in the frame's
trailing auxiliary data (`cargo xtask eac3` reports it, on all six). So
`Encoder::carry_skip_field` takes the bytes of an Evolution frame behind its
sync word and puts them there in the next frame, the frame header turning its
skip fields on.

## Against FFmpeg

The 30-second 5.1 film excerpt of [docs/ac3.md](ac3.md), `-c:a eac3` at the
same rates, both decoded by FFmpeg (and ours with `-xerror -err_detect
explode`, without complaint):

| Programme | Rate | `hz-ac3` | `ffmpeg -c:a eac3` |
|---|---|---|---|
| 5.1, 48 kHz | 768 kb/s | 40.06 dB | 40.17 dB |
| 5.1, 48 kHz | 640 kb/s | 36.88 dB | 36.85 dB |
| 5.1, 48 kHz | 384 kb/s (coupled) | 27.44 dB | 27.44 dB |
| 2.0, 48 kHz | 192 kb/s (coupled) | 29.50 dB | 29.51 dB |

The frame sizes are the same frame for frame, and the SNR offsets and frame
exponent codes the same in all but four to ten frames of 969. The 0.11 dB at
768 kb/s is a handful of starved frames — dialogue transients coded on D45
throughout at a low offset — where a last-bit difference between the two
transforms moves bits from one bin to another: per frame the two encoders are
worse and better about equally often (278 and 259 frames), the median ratio of
their errors 1.0004, and at 640 kb/s it falls the other way.

Our streams decode through the workspace's `eac3` crate too: `cargo run -p
xtask --features eac3-oracle -- ac3 stream.ec3 --reference in.f32` takes both
formats.

## The graft

Before any joint object coding of our own: does the E-AC-3 we write carry
objects? `xtask graft` takes a reference stream, decodes its 5.1 core, encodes
it again with `hz-ac3` — fed 256 samples early so that our frame *k* decodes
to the reference's frame *k* — and gives every frame back the reference frame's
Evolution frame byte for byte in block 2's skip field. Then both streams go
through the workspace's object decoder, and each object rebuilt from our core
is compared with the same object rebuilt from the reference's:

```bash
cargo run --release -p xtask --features eac3-oracle -- graft reference.ec3 --out grafted.ec3
```

| Reference | Rate | Core | Objects |
|---|---|---|---|
| film, 60 s, 15 objects | 768 kb/s | 35.19 dB | 35.37 dB (27.5 to 39.0) |
| film, 30 s, 15 objects (7 active) | 768 kb/s | 38.50 dB | 41.25 dB |
| sample, 11 objects | 640 kb/s | 34.68 dB | 34.68 dB (32.5 to 38.2) |
| fixture, 1 s | 768 kb/s | 67.60 dB | 67.60 dB |

Every frame of every stream carries its objects, and they come back as close to
the reference's as the core does: the frames are where a decoder looks, they
line up sample for sample, and the parameters still fit a core coded again. The
grafted streams decode in FFmpeg strictly, and `xtask eac3` reads every
Evolution frame in them back as written.

What the graft does not show: the protection fields come across unchanged — a
digest over audio that is no longer the audio — and no public decoder checks
them; whether a receiver does is the open risk in PLAN.md. And it says nothing
about our own parameters, which are phase B3.

## Not done

- A dependent substream, and so 7.1 and the seven-channel downmix
  configurations of joint object coding.
- Spectral extension, enhanced coupling and the adaptive hybrid transform:
  the tools that matter below 640 kb/s.
- Mixing and informational metadata (the downmix levels, `bsmod`), and as in
  AC-3 no dynamic range words.
- Frames of one, two or three blocks, which FFmpeg chooses above 1024 kb/s.
