# Loudness, and how it was checked

[`hz-analysis`](../crates/hz-analysis) implements ITU-R BS.1770-4 loudness,
EBU Tech 3342 loudness range, and BS.1770-4 Annex 2 true peak.

## Checked twice, in different ways

Two kinds of test, because they catch different failures.

**Against arithmetic.** For a steady tone the answer is a closed form —
`-0.691 + 10·log₁₀(Σ G·k²·A²/2)`, with `k` the K-weighting gain at that
frequency — so the gating and summing can be checked against the filter rather
than against a remembered number. The filter design is checked against the
coefficients BS.1770-4 publishes at 48 kHz, and against its own response at
four sample rates, since designing it rather than tabulating it is the only way
to serve 96 and 192 kHz.

That catches an implementation that disagrees with arithmetic. It cannot catch
one that agrees with arithmetic and disagrees with every other meter — about
where a window starts, or what a gate does at the edges.

**Against another meter.** So the same files go through FFmpeg's `ebur128`:

| Material | I (ours / ffmpeg) | LRA | True peak |
|---|---|---|---|
| 1 kHz stereo tone, 20 s | −21.1 / −21.1 | 0.0 / 0.0 | −21.1 / −21.1 |
| Pink noise, stereo, 30 s | −25.1 / −25.1 | 0.1 / 0.1 | −14.5 / −14.3 |
| The same at −20 dB | −41.1 / −41.1 | 0.0 / 0.0 | −41.1 / −41.1 |
| Pink noise, **5.1**, LFE excluded, surrounds weighted | −22.6 / −22.6 | 0.1 / 0.1 | −8.4 / −8.3 |
| 60 s alternating 10 s loud / 10 s quiet | −20.7 / −20.7 | 20.1 / 20.0 | −10.2 / −10.1 |

Integrated loudness agrees to the printed resolution in every case, the 5.1 one
included — which is the one that exercises the channel weights and the LFE
exclusion rather than just the filter.

```bash
cargo xtask loudness --lfe 3 --surround 4,5 file.wav
ffmpeg -i file.wav -af ebur128=peak=true -f null -
```

## Where the two disagree, and why that is expected

True peak differs by 0.1 to 0.2 dB. That is not a defect in either: BS.1770-4
requires oversampling by at least four and offers an interpolator, but does not
require *that* interpolator, and no widely used implementation carries it —
FFmpeg resamples to 192 kHz with its general resampler, and this crate uses a
Blackman-Harris windowed sinc of twelve taps per phase. Two true-peak meters
therefore agree to about a tenth of a decibel by construction. Loudness range
differs by 0.1 LU for the same kind of reason: Tech 3342 fixes the gates but
leaves the percentile method to the implementer.

## Two things worth knowing before reading a number

**A step overshoots, and that is real.** A file that opens at full level has a
true peak above its largest sample, because the band-limited reconstruction of
a step goes past it. It looks like a bug the first time.

**This measures a presentation, not a master.** BS.1770 is defined over a
channel-based layout with fixed per-channel weights. Pointing it at an
object-based master and weighting every object by one is not the
recommendation, and is not what happens here: measuring a master means
rendering a presentation first and measuring that.

That is what the harness measures: `cargo xtask loudness` reads a rendered
file — from whichever renderer made it, with `--lfe` and `--surround` naming
the channels BS.1770 treats differently — and is checked against FFmpeg
reading the same file.

## Measuring dialogue, not the programme

BS.1770 measures everything that is there. `dialnorm` is about the dialogue
only, which is a different measurement over the same blocks: see
[docs/speech.md](speech.md) for the gate that selects them, what it costs, and
where it fails.
