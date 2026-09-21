# harlettizer

An open encoding engine for immersive audio, and the encoder counterpart to
[`harletty`](https://github.com/harletty/harletty-bridge). Where harletty
*decodes* immersive bitstreams into masters, harlettizer *encodes* masters into
bitstreams — today a **lossless TrueHD/MLP stream carrying object audio
metadata**, written from an object master and checked bit for bit through
decoders that share no code with it.

Pure Rust, headless, GPL-3.0-or-later.

```bash
harlettizer convert programme.atmos programme_adm.wav                  # master set -> ADM BW64
harlettizer encode  programme.atmos --out programme.thd --cluster 12   # the immersive bitstream
```

## What it does

Two commands, each taking a master file set (`.atmos` + `.atmos.audio` +
`.atmos.metadata`) or an ADM BW64 file.

| | |
|---|---|
| `convert` | One container to the other, either direction |
| `encode` | The programme as a TrueHD stream: its elements coded losslessly, its object metadata carried in the access units, its objects clustered into the twelve to sixteen elements a delivery stream holds, and — on request — real 2.0, 5.1 and 7.1 folds inside the stream for decoders that stop early |

**Everything an encode writes is decoded back and compared, not inspected.**
The test suite decodes every presentation of every stream it writes through a
reference decoder in the test tree and verifies the lossless check each restart
header carries. The development harness does the same through `harletty` (the
`truehd` crate) and, up to eight channels, through FFmpeg. On a real programme
the twelve elements come back with a largest difference of zero, and every
object comes back where the master put it, at the sample the master named.

### Where it stands

- **Channel-based streams** — mono, stereo, 5.1 and 7.1, within a few per
  cent of FFmpeg's TrueHD encoder either way, measured on slices of a
  programme:

  | | mono | stereo | 5.1 | 7.1 |
  |---|---|---|---|---|
  | size, as a ratio of FFmpeg's | 0.96 | 0.94 | 0.97 | 1.06 |

- **Immersive streams** — nine to sixteen elements over four substreams, the
  object metadata read back by a real decoder as the programme it describes,
  and a real programme at about the size of its reference stream. With
  `--presentations` the stream carries genuine folds, a few per cent larger
  than without.
- **Speed** — about 3 ms per channel-second of audio at full effort on a
  desktop machine: four minutes of a twelve-element programme in 17 s on
  sixteen cores.
- **Not there yet** — E-AC-3 with joint object coding, AC-3 and AC-4 are in
  [PLAN.md](PLAN.md) and nothing of them is written. There is no release; build
  from source.

## Building

```bash
cargo build --release -p hz-cli
./target/release/harlettizer --help
```

Rust 1.87 or later. Pure Rust: no C toolchain, no vendor SDK, no git
dependency.

Each release also carries the built binary for Linux (x86-64), Windows
(x86-64) and macOS (Apple silicon), zipped with the licences it ships under.

## Usage

### `convert`

```bash
harlettizer convert programme.atmos programme_adm.wav   # master set -> ADM BW64
harlettizer convert programme_adm.wav programme.atmos   # ADM BW64 -> master set
```

The direction is taken from the input. `--presentation N` picks which
presentation of a master set to convert (default 0). Two things a master set
says have no place in ADM and are reported when they are dropped: its language,
and its frame rate, since ADM times are absolute.

### `encode`

```bash
harlettizer encode programme.atmos --out programme.thd --cluster 12 --fold-depth 18 --presentations
```

Four minutes of a programme, sixteen objects and an LFE into twelve
elements:

```
  programme    16 objects and an LFE, 12 elements
  encoded      288000 access units, 11520000 samples
  metadata     3797 payloads, one every 76 units
  fold         0.0000 mean over the presentations, 0.000 at its worst, as a fraction of the object's own gains
  depth        elements rounded to 18 bits
  drc          from the measured curve, per presentation, restated every 128 units
  wrote        programme.thd
```

| Option | Default | What it does |
|---|---|---|
| `--out <FILE>` | required | Write the stream here |
| `--cluster <ELEMENTS>` | | Fold the master's objects into this many elements — twelve, fourteen or sixteen — rather than giving each object one of its own. Without it the stream is one element per object, which needs a master that has already been clustered. The LFE is a bed, takes the first element untouched, and is not clustered |
| `--fold-depth <BITS>` | `20` | Round the folded elements to this many bits. A mix is a sum over real weights, so its low bits are the noise of the multiplication, and a lossless coder carries them anyway; twenty is worth about a third of the stream against twenty-four and puts the rounding near −120 dBFS. Seventeen is the floor. Match it to the source master. Only means anything with `--cluster` |
| `--fold-search <PASSES>` | `4` | Passes of the search that places each element where the fold's own cost is lowest. Zero declines it. Worth about a fifth of what the fold costs and a twentieth of the stream. Only means anything with `--cluster` |
| `--beds <RULE>` | `auto` | What a fold does with a bed channel other than the LFE: `pinned`, an element of its own at its speaker's place; `free`, an object of the bed class sharing the elements with the rest; or `auto`, pinned while every object can still have an element beside the bed and free otherwise. Only means anything with `--cluster`; without it a bed other than the LFE is refused |
| `--dialnorm <DB>` | `-31` | The programme's dialnorm, which sets the floor under what an object has to carry to be heard at the playback level: full scale at 105 dB SPL less the gain a decoder applies to bring the dialogue to −31, and the threshold of hearing under that — −102.6 dBFS at the reference. Below it an object seeds nothing and sets no worst case. Only means anything with `--cluster` |
| `--headroom <RULE>` | `limit` | How the fold keeps its elements inside the codec's domain: `limit`, a limiter with one gain for every element, ahead of the peak; `bound`, the fit bounding an element's coherent peak, which costs error and does not take every clip; `both`; or `off`, which clamps and counts the clips as it did. Only means anything with `--cluster` |
| `--loudness <RULE>` | `flat` | How a block's power is weighed before it steers the fold: `flat`, the plain power a meter reads; `kweighted`, through the K filter of BS.1770; or `perceptual`, what each object adds to the loudness of the scene, band by band, with what is near it masking it. Flat is what every measurement in `docs/clustering.md` was made under, and the perceptual rule did not beat it on the metric; it is there to be listened to, at about a third more encoding time. Only means anything with `--cluster` |
| `--presentations` | off | Make the stream carry real folds: a decoder stopping after any substream is handed a 2.0, a 5.1 or a 7.1 rather than the leading elements. A full decode is unchanged and still lossless. Off by default because it cannot always be done — the cascade that builds the hierarchy amplifies what it is given and the codec's domain is twenty-four bits, so a programme mixed near full scale is written without one, and the summary says so |
| `--drc <MODE>` | `measured` | The dynamic range word each presentation states: `measured`, a gain in decibels, or `off`. Metadata only; a decoder asked for full range ignores it and the samples are unchanged |
| `--fast` | off | Search less hard: one second-filter design, decided at every other restart. About a quarter off the time of an encode for a fraction of a per cent of stream |
| `--frames <N>` | | Stop after this many frames of audio |
| `--progress` | off | One `progress <n>%` line on standard error each time the whole percent changes — at most a hundred and one lines however long the programme — for a caller driving a bar. Standard output is untouched and the stream is byte-identical either way |

What the summary says:

- **programme** — what the master holds and how many elements the stream will
  carry. Nine is the fewest the format's shape allows and sixteen the most; a
  smaller programme is padded with inactive elements the syntax has a flag for.
- **metadata** — how many object audio metadata payloads went out. One goes out
  when some element's state changes, and otherwise every 128 access units.
- **fold** — what the clustering cost: how far every object lands from where
  the mix put it once folded into the elements, measured from the metadata
  alone over every presentation the stream will be played through. An encoder
  that folds a scene and does not say what it cost is asking to be trusted.
- **clipping** — an element is a sum, and a sum of objects that agree is louder
  than any of them. Samples past the codec's twenty-four bits are clamped rather
  than wrapped, and counted here when there are any.
- **presentations** — with `--presentations`, whether the stream carries the
  folds, or why an interval could not.

Two environment variables make the encoder talk, for diagnosis: `HZ_TIME=1`
times its phases and prints them at the end, and `HZ_FOLD=1` says at every
restart how the presentations were arranged and why a build was refused where
one was.

To look at what was written:

```bash
harletty decode programme.thd --presentation 3 --output-path programme_back   # the elements, as a master set
cargo xtask thd programme.thd                                                 # the stream's structure and integrity
```

### What a stream carries

Four substreams, each a presentation a decoder may stop at: two channels, six,
eight, and the elements. Without `--presentations` the first three are the
leading elements; with it they are the folds themselves, and the last
substream says how to turn them back into elements. A restart header every 128
access units, the object metadata beside the audio, a dynamic range word per
presentation, and the high-resolution timing field that lets a decoder joining
mid-stream say where in the programme it is.
[docs/mlp-state.md](docs/mlp-state.md) lists every constant and decision in
force; [docs/mlp.md](docs/mlp.md) is the notebook of how they were arrived at.

## Development

```bash
cargo build --all-targets
cargo test --all
cargo fmt --all --check
```

CI gates formatting, the build and the full test suite; clippy is advisory.
The test suite needs no external decoder and no external material.

`cargo xtask` is the development harness — measurements and checks that need
real material or an external decoder:

| Command | What it does |
|---|---|
| `mlp` | Encode a file losslessly and check a decoder gets it back exactly — through FFmpeg up to eight channels, and through `harletty`, found on the `PATH` or named by `HARLETTY` |
| `thd` | Read a TrueHD stream's structure and check its integrity |
| `cluster` | Fold a master set's objects into the elements a bitstream carries, and say what it cost |
| `bench` | Render a master and each rule's fold of it on 7.1.4 as blinded stimuli for a listening session, and score a filled-in sheet into a ranking |
| `drc` | What gain a shipped stream states, and when it changes it |
| `loudness`, `speech` | Measure, or report on the speech gate, to hold next to another implementation |
| `roundtrip`, `container`, `adm` | Read every master set under a directory and write it back; rewrite audio containers; report what in a BW64 file's ADM was not understood |
| `make-master` | Write a small synthetic master file set to convert against — pure tones, a broadband scene of a voice, a rumble and effects, or the same with the effects starting and stopping; with every Nth object snapped or confined to the front, for scenes with classes; over an LFE, a left channel and the LFE, or a 7.1.2 bed |
| `corpus` | Survey a directory of master sets and report what they can be used to test |

The workspace is `hz-core` (formats and geometry), `hz-io` (the two
containers), `hz-analysis` (loudness, speech, dynamic range), `hz-render`
(layouts, the measured fold model, and the panner the clustering metric judges
the immersive presentation on), `hz-cluster` (objects into elements), `hz-meta`
(object audio metadata), `hz-mlp` (the lossless encoder) and `hz-cli`.
The codec crates never depend on each other; only `hz-cli` knows all of them.
See [CONTRIBUTING.md](CONTRIBUTING.md).

## Documentation

| | |
|---|---|
| [docs/encode.md](docs/encode.md) | The `encode` command: what a real programme did, the progress protocol, what `--fast` costs |
| [docs/mlp-state.md](docs/mlp-state.md) | What the MLP encoder writes today — every constant and decision in force |
| [docs/mlp.md](docs/mlp.md) | The lossless encoder's notebook: what was tried, what it measured, what was put down |
| [docs/clustering.md](docs/clustering.md) | Clustering objects into elements, and the metric that judges a clustering |
| [docs/clustering-next.md](docs/clustering-next.md) | What the clustering still lacks, the public sources for each quality, and how each lever is to be measured |
| [docs/fold.md](docs/fold.md) | How a shipped stream folds an object onto a channel presentation, measured |
| [docs/objects.md](docs/objects.md) | Object audio metadata, and what a payload can and cannot say |
| [docs/drc.md](docs/drc.md) | Dynamic range metadata |
| [docs/loudness.md](docs/loudness.md), [docs/speech.md](docs/speech.md) | Loudness and true peak, and the speech gate behind `dialnorm` |
| [docs/adm.md](docs/adm.md), [docs/master-format.md](docs/master-format.md) | The two containers, as implemented and as observed |
| [docs/provenance.md](docs/provenance.md) | Every piece of code, table or constant that came from another project |
| [PLAN.md](PLAN.md) | The phased plan, and what each phase was judged on |

## Legal

**GPL-3.0-or-later.** That choice is what lets FFmpeg's LGPL-2.1 encoders be
ported here; what was taken from FFmpeg, from the `truehd` crate (Apache-2.0)
and from the EBU ADM Renderer (BSD-3-Clause) is recorded file by file in
[docs/provenance.md](docs/provenance.md), and [LEGAL.md](LEGAL.md) says which
sources are open to this project and which are closed.

**Patents.** The formats implemented here are covered by patents held by their
format owners. Writing and distributing an independent implementation is
lawful where this project is developed; *using* it may require a licence
depending on jurisdiction and use. This project makes no claim that using it
is patent-free, and follows the posture of comparable projects: ship the
source, let each user assess their own obligations.

**Trademarks.** Format names are used descriptively and only where necessary.
The output is compatible with decoders implementing the published
specifications; it is not certified or approved by any format owner, and
bit-identical output to any proprietary encoder is neither achievable nor the
target. Decoder interoperability and audible parity are.
