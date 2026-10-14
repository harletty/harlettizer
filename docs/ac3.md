# AC-3 output

```bash
harlettizer ac3 programme.atmos --out programme.ac3                    # 5.1 at 448 kb/s
harlettizer ac3 programme.atmos --out programme.ac3 --bitrate 640      # 5.1 at 640 kb/s
harlettizer ac3 programme.atmos --out programme.ac3 --layout 2.0       # stereo at 192 kb/s
```

Takes a master set or an ADM BW64 file and writes an AC-3 elementary stream
(ATSC A/52), 5.1 or stereo, at any of the format's nineteen rates from 32 to
640 kb/s, at 48, 44.1 or 32 kHz — the sample rate is the master's. A decoder's
output lags the input by 256 samples.

This is phase B1 of the lossy track (see [PLAN.md](../PLAN.md)): the transform,
exponents and bit allocation that E-AC-3 and then joint object coding are built
on. On its own it is what a legacy decoder plays.

## The render

AC-3 has no objects, so the master is rendered: bed channels routed to their
speakers, objects panned on the room's cube — the same panner `iamf` uses —
onto **7.1**, and 7.1 folded to the layout asked for by the fold measured off
shipped TrueHD streams (`hz_render::fold::presentation_matrix`), which is the
one this project's own TrueHD presentations carry. The cube has no place for a
5.1 surround at ±110°; the fold is how a 5.1 listener hears the same master
from the lossless stream, so the two agree. Stereo leaves the LFE out, as a
decoder folding 5.1 to stereo does.

The loudness measured on the render, before the codec, is reported beside the
dialnorm the stream states (`--dialnorm`, −31 unless asked). Nothing sets the
one from the other yet.

## The encoder

`hz-ac3` is a port of FFmpeg's AC-3 encoder (`libavcodec/ac3enc*.c`), and on
purpose makes the stream FFmpeg makes before it makes any decision of its own:

- **transform** — the 512-point MDCT of A/52 §8.2.3 with its Kaiser-Bessel
  window (α = 5), written from the standard's definition and held to it by a
  test, as a 128-point complex FFT between two rotations;
- **coupling** — the full-bandwidth channels summed over the high band from
  the start band FFmpeg picks for the mode and rate (on for 5.1 up to 512 kb/s,
  for stereo up to 256), per-band coordinates resent where they move;
- **stereo rematrixing** — sum and difference per band wherever either has less
  energy than both sides;
- **exponents** — resent where they move by more than 500 summed over the
  block, on a coarser grid the longer they stand (D15, D25, D45);
- **bit allocation** — A/52's parametric model, integer for integer what the
  decoder runs, and the constant-rate search for the highest SNR offset whose
  mantissas fit;
- **audio bandwidth** — FFmpeg's choice per rate and channel count: 20.3 kHz at
  448 kb/s for 5.1, for example.

Not yet: dynamic range words (`dynrng`, `compr`) — the stream carries none, so
a decoder applies no compression — block switching for transients, and the
extended bitstream information (Lt/Rt and Lo/Ro downmix levels).

FFmpeg's choices that change the stream are kept where they look like
accidents too — the coupling coordinates pooled over the *last* channel's
runs, the exponent steps that reach one bin past the coupling channel — so
that the first comparison is of a port, and so that each departure later is a
measured one.

## Against FFmpeg

A 30-second 5.1 film excerpt (the core of an E-AC-3 stream, decoded), and a
stereo fold of it, coded by both encoders at the same settings and decoded by
FFmpeg; the SNR against the source over the whole excerpt, after the 256-sample
delay both have:

| Programme | Rate | Coupling | `hz-ac3` | `ffmpeg -c:a ac3` |
|---|---|---|---|---|
| 5.1, 48 kHz | 640 kb/s | off (neither couples) | 36.30 dB | 36.31 dB |
| 5.1, 48 kHz | 448 kb/s | default | 29.42 dB | 29.42 dB |
| 5.1, 48 kHz | 448 kb/s | off | 28.71 dB | 28.71 dB |
| 5.1, 48 kHz | 384 kb/s | default | 26.90 dB | 26.91 dB |
| 2.0, 48 kHz | 192 kb/s | default | 28.62 dB | 28.62 dB |
| 2.0, 44.1 kHz | 192 kb/s | default | 29.01 dB | 29.02 dB |
| 1.0, 32 kHz | 96 kb/s | — | 27.34 dB | 27.34 dB |

The streams are not byte-identical — the transforms round differently in the
last bit of a float, which now and then moves an exponent and with it the
allocation — but close: at 44.1 kHz in stereo, 417 of 744 802 bytes differ.
SNR is not a perceptual measure; what it shows here is that the two encoders
make the same decisions.

The same streams decode through the workspace's own decoder too — the `eac3`
crate's AC-3 path, written independently of FFmpeg and the one that decodes
the legacy core of a dual-core E-AC-3 stream — every syncframe of every
configuration above, with the same agreement: 36.91 dB against FFmpeg's
stream's 36.92 at 640 kb/s, 29.64 against 29.64 at 448.

```bash
cargo run --release -p xtask --features eac3-oracle -- ac3 out.ac3 --reference in.f32
```

Speed: the 30-second 5.1 excerpt codes in 0.21 s, about 140 times real
time, against FFmpeg's 0.13 s with its assembly; nothing here is vectorised by
hand yet, and the allocation search reruns the pointer computation at every
SNR step as FFmpeg's does.

Every syncframe carries both CRCs (checked as a decoder checks them, in the
tests), and in a debug build every frame holds the bitstream writer to the
bit count the allocation planned on.

To repeat the comparison: `cargo run --release -p hz-ac3 --example raw_to_ac3
-- in.f32 out.ac3 RATE CHANNELS KBPS` codes raw interleaved float PCM in WAV
order (`NO_COUPLING=1` turns coupling off), and `ffmpeg -f f32le -ar RATE -ac
CHANNELS -i in.f32 -c:a ac3 -b:a KBPSk out.ac3` is the other side.
