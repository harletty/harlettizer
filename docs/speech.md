# The speech gate, and how it was calibrated

`dialnorm` tells a decoder how loud the **dialogue** in a programme is, so that
playback can normalise to it. Measuring the whole programme answers a different
question: a programme whose dialogue sits at −31 LKFS and whose action sits at
−18 measures around −20 overall, and a decoder told −20 plays the dialogue too
quietly to follow.

So the loudness that feeds `dialnorm` is measured over the parts of the
programme that contain speech. The reference encoder does this with a
proprietary, undocumented speech gate. [`hz-analysis::speech`](../crates/hz-analysis/src/speech.rs)
is not a reimplementation of it — it cannot be, nothing about it is published —
it is an open substitute whose behaviour is measured and written down here.

## What it looks at

Every 10 ms it takes the last 80 ms of the analysis signal and asks four
questions. All four have to answer yes.

| Feature | What it rejects |
|---|---|
| Level above an adaptive noise floor | Room tone, dither, silence |
| Energy fraction in 300 Hz – 3.4 kHz | Rumble, broadband effects, bass-heavy music |
| Cepstral peak prominence | Noise, applause — anything not periodic |
| 4 Hz envelope modulation | Held notes and steady noise |
| Positive spectral flux in the speech band | A tone, which passes all of the above |

The analysis signal is the **centre channel** where the layout has one, and the
mid of the front pair otherwise. That is not a shortcut; it is most of the
discrimination. In a theatrical mix the dialogue is in the centre and the music
is not, so the gate starts from a signal that is already mostly what it is
looking for.

Voice-like frames are then smoothed in time — three consecutive frames open the
gate, and it stays open across 250 ms of silence, which is the gap between two
words — and reduced to one flag per 100 ms, which is the grid BS.1770 already
accumulates on.

A 400 ms block sets the dialogue level only if every sub-block in it is inside
the open gate, a quarter of its frames are voice-like in their own right, and
its first and last sub-block each hold a voice. Those last two conditions were
put there by measurements, and [what they fixed](#three-things-the-measurements-changed)
is below.

## The thresholds are ours

Nothing specifies them. They were set by measuring the features on labelled
material and taking the middle of each plateau, not the edge of a cliff:

```bash
cargo xtask speech --speech en.wav --other pink.wav --other strings.wav --features
```

| Threshold | Value | Sweep |
|---|---|---|
| Above the noise floor | 8 dB | flat from 4 to 12 dB |
| Speech band ratio | 0.50 | 0.35 → 16 % false alarms; 0.45–0.55 flat; 0.65 loses coverage |
| Cepstral peak prominence | 1.0 dB | 0.6 → 15 %; 0.8–1.2 flat; 2.0 loses half the coverage |
| 4 Hz modulation | 2.0 dB | 1.0 → 8.8 %; 2.0 → 4.9 %; 3.0 starts costing coverage |
| Spectral flux | 0.02 | 0.0 → 15.4 %; 0.02 → 8.8 % |

## What it does

Frames the gate held open, over material labelled by construction:

| Material | Gate open |
|---|---|
| English speech (synthesised) | 93.0 % |
| French speech (synthesised) | 94.4 % |
| A lower, slower voice | 88.6 % |
| A programme's centre channel, 40 s | 51.8 % |
| Pink noise | 0.0 % |
| A held 1 kHz tone | 0.0 % |
| A sustained chord with tremolo | 0.0 % |
| Rhythmic filtered noise at 4 Hz | 0.0 % |
| A moving harmonic line — a string part | 0.0 % |
| Applause | 0.0 % |
| **Sung vowels holding pitched notes** | **4.9 %** |

The programme's 51.8 % is not a failure: a centre channel carries music,
effects and silence as well as dialogue, and roughly half of a dialogue-heavy
excerpt being dialogue is the expected answer rather than a suspicious one.

## What it recovers, which is the number that matters

Detection rates are a proxy. The measurement the gate exists to make is a
level, so the acceptance test lays speech at a known loudness against other
material at a different known loudness and asks what comes back.

| Speech | Against | Programme | Dialogue | Error |
|---|---|---|---|---|
| −27 | silence | −27.02 | −26.60 | **+0.40 LU** |
| −27 | pink noise at −15 | −17.00 | −26.38 | **+0.62 LU** |
| −27 | a chord at −12 | −12.42 | −26.03 | **+0.97 LU** |
| −31 | applause at −12 | −12.17 | −29.54 | **+1.46 LU** |
| −23 | sung vowels at −23 | −22.93 | −23.53 | **−0.53 LU** |
| −27 | a string part at −15 | −16.98 | −21.58 | **+5.42 LU** |

The programme column is what a meter without a gate would report, and it is
wrong by up to 19 LU — which is the whole argument for having a gate.

The floor of +0.40 LU against silence is the gate's own bias and not leakage:
it keeps the blocks that hold a voice and drops the gaps between phrases, so
its answer is slightly above the loudness of the speech including its gaps.
`dialnorm` is quantised in 1 dB steps, so that bias costs nothing.

The last row is the failure mode, and it is stated rather than tuned away.

## Where it fails

**Pitched harmonic material whose fundamental sits in the speech band.** A
voice and a bowed string are both periodic, both band-limited to roughly the
same place, and both move. The features here do not separate them, and no
combination of them will: this is the boundary of what feature-based
speech/music discrimination does, and it has been the boundary for thirty
years.

What limits the damage in practice is the analysis signal. The gate listens to
the centre channel, and a theatrical mix does not put the strings there. The
string row above is a synthetic worst case fed straight down the middle.

The honest summary: **against noise, effects, applause, tones and sustained
music the gate is worth 0.4 to 1.5 LU; against a melodic line in the centre
channel it is worth nothing and will read high.** A programme where that
matters should be measured with a gate that knows more than this one does.

## Three things the measurements changed

Each of these was found by a number moving, not by reading the code.

**The cepstrum was being taken over the wrong thing.** Computed across the
whole spectrum of a 48 kHz signal, the harmonic ripple — which lives in the
bottom fifth of the bins — is spread across all of them and the peak all but
disappears: real speech measured 2 dB where a synthetic vowel gives 13. Taking
it over a fixed 512-point grid spanning 0–5 kHz fixed that, and had the side
effect of making the measure, and so its threshold, independent of the sample
rate.

**The noise floor was a minimum, and a minimum is set by one frame.** Anything
with gaps in it — music with a note attack every bar — drags the floor to
nearly nothing and leaves the level test permanently satisfied. On a mixture of
speech and a moving harmonic line that put two thirds of the music into the
dialogue measurement. It is now the 10th percentile of the frame level over
1.5 s, read off a one-decibel histogram: constant space, one increment per
frame.

**The hangover was carrying the loud material in.** The gate stays open across
the gaps inside speech, which is what it is for, but a block at the end of a
line is mostly hangover and holds one syllable of actual voice. With 19 dB
between the dialogue and what follows it, five such blocks out of a hundred and
eighteen moved the answer by 4 LU. Requiring a voice in the first and last
sub-block of every measured block brought that case from +4.19 LU to +1.46.

## Still to do

The delta against the reference engine has not been measured: it needs a
licensed run of the engine, and none has been available to this project.
See [LEGAL.md §3.1](../LEGAL.md).
