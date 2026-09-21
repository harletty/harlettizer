# The master file set, as observed

Notes taken while writing the phase 1 reader and writer. The format is not
published in the detail an implementation needs, so this records what real
masters actually contain — and, where the two differ, what the tooling does
versus what the format requires.

A master set is three files sharing a stem:

| File | Contents |
|---|---|
| `<stem>.atmos` | Config: presentation, bed channels, object IDs, and the names of the other two files |
| `<stem>.atmos.metadata` | The event stream: one entry per object per update |
| `<stem>.atmos.audio` | The audio, in a CAF container |

All three are read and written by [`hz-io`](../crates/hz-io).

## The config document

Top level: `language` (optional — often absent), `version`, `presentations`.

Per presentation, in the order they are written: `type`, `simplified`,
`metadata`, `audio`, `offset`, `ffoa`, `fps`, `scNumberOfElements`,
`scBedConfiguration`, `creationTool`, `creationToolVersion`, `sourceCodec`,
`downmixType_5to2`, `51-to-20_LsRs90degPhaseShift`, `warpMode`, `trimMode`,
`bedInstances`, `objects`.

Enumerated values seen or documented:

- `type`: `home`, `cinema`
- `fps`: `23.976`, `24`, `25`, `29.97`, `29.97df`, `30`
- `sourceCodec`: `TrueHD`, `EAC3-JOC`, `DTS:X-7.1.4`, `DTS:X-7.1.5`,
  `DTS:X-9.1.4`, `DTS:X-7.1+8`
- `warpMode`: `normal`, `warping`, `ProLogicIIx`, `LoRo`
- `downmixType_5to2`: `LoRo_Stereo`, `LtRt_ProLogic`, `LtRt_PLII`

`trimMode` is a map of nine downmix cases (`NoSurroundsNoHeights` …
`ManySurroundsManyHeights`), each an optional set of five trims. Exactly one
example has been seen and it is the empty map, `{}`, so the scalar spelling of
a populated one is modelled from the rest of the format rather than observed.

## The event stream

`sampleRate`, then `events`. Each entry, in write order: `ID`, `samplePos`,
`active`, `pos`, `snap`, `elevation`, `zones`, `size`, `size3D`, `decorr`,
`importance`, `gain`, `rampLength`, `trimBypass`, `dialog`, `music`,
`distance`, `screenFactor`, `depthFactor`, `headTrackMode`,
`binauralRenderMode`.

`active: false` is read as a gain of nought: an object the master switched off
is silent for every encode — plain, `--cluster` and `--overlay` alike — and an
overlay never pans a source onto it. The stream states it as an element with a
silent gain, since the payload's own inactive flag is what a plain encode uses
for padding and not for a master's choice.

`zones`: `all`, `no back`, `no sides`, `center back`, `screen only`,
`surround only`.

Three fields — `size3D`, `decorr`, `distance` — are in the format and have
never been seen in a real master. They are modelled, and untested by real data.

**It is a delta stream.** An entry states what changed; what it omits stays in
force. A reader that fills omitted fields with defaults re-asserts values that
were never sent, which is why every field in the model is an `Option` and why
nothing is defaulted on read.

## Three traps in the encoding

### `gain: -inf`

Silence is written as the bare token `-inf`. Any YAML 1.2 parser reads that as
a string, not a float, so a field typed as a plain number rejects real files.
Rust's `f64` parses and `Display`s `-inf` in exactly this spelling, so one
lenient scalar type reads and writes it without a special case.

### `binauralRenderMode: off`

`off` is a **string** in YAML 1.2 and a **boolean** in YAML 1.1. The format's
tooling writes it bare, so we do too, and we do not quote it — quoting would
round-trip correctly but make every file we write look foreign. A YAML 1.1
parser (PyYAML, say) will read it as `false`; that is already true of every
master in circulation, and is worth knowing before debugging a tool that
reads these with Python.

### Two number styles, one format

A position is written `[-1, 1, 0]` and a size is written `0.0`. Both parse
identically; the difference is how the tooling formats them, not what the
format demands. `hz-io` carries two scalar types to match — `Num` for
positions, sizes-3D and gains, `Real` for sizes, offsets, importance and
factors — because matching costs one type and makes our output
indistinguishable from every other master.

## Component names are not to be trusted

A config names its own metadata and audio files, and in a third of the masters
seen that name is stale — left behind by an earlier rescan. The reader resolves
the name first and falls back to the config's own stem, and it keeps the
distinction in the type (`Component::ByConvention`) so callers can report a
stale reference rather than silently paper over it.

## The audio component

A CAF file, big-endian throughout:

```
caff  version 1  flags 0
desc  size 32   sampleRate f64 | 'lpcm' | flags | bytesPerPacket
                framesPerPacket | channels | bitsPerChannel
data  size i64  editCount u32, then interleaved samples
```

Observed in a set produced by the decoder: 48 kHz, `lpcm`, flags 0
(big-endian signed integer), 24 bits, 21 channels, 63 bytes per packet, one
frame per packet. No `chan` chunk — the channel layout is not written, because
the config already says what each ID is.

Two things the reader has to get right:

- The `data` chunk starts with a four-byte edit count **before** the samples.
  Treating the chunk size as the sample count overruns by one frame's worth
  and shifts every channel.
- A declared size of −1 means "to the end of the file", which is what a
  writer that never seeks back leaves behind.

Chunks other than `desc` and `data` are read and carried through verbatim
rather than skipped. A master's channel layout can live in one of them, and
dropping it on a rewrite is silent corruption that would only surface in
somebody else's decoder. Anything larger than a megabyte is refused instead of
carried, since no metadata chunk is that big and quietly dropping it is the
failure mode being avoided.

## The other container: RIFF, RF64, BW64

An ADM master is a BW64 file, which is RF64 with a different signature. Both
exist because RIFF's 32-bit size fields cannot describe a file past four
gigabytes: the successors set every such field to `0xFFFFFFFF` and put the real
riff size, data size and frame count in a `ds64` chunk.

Details that bite:

- Chunks are word-aligned and the pad byte is **not** counted in the chunk
  size. Reading the next chunk from the uncorrected offset finds noise.
- `WAVE_FORMAT_EXTENSIBLE` (tag `0xFFFE`) hides the real format tag in the
  first two bytes of the subformat GUID, and carries the channel mask that is
  the only place a plain WAV says which speaker each track feeds.
- `chna` and `axml` are ordinary chunks. A reader that skips what it does not
  understand throws away the entire ADM description.

Verified by rewriting files written by ffmpeg — RIFF 5.1 extensible 24-bit,
RF64 mono 16-bit, and 7.1 float with a `fact` chunk — and comparing byte for
byte:

```bash
cargo xtask container path/to/file.wav
```

## What real masters proved

Every struct in `hz-io` is `deny_unknown_fields`. Reading every master to hand
with zero failures is therefore not a smoke test but a completeness proof: a
field the model did not know about would have stopped the run. It caught four
on the first pass — `sourceCodec`, `warpMode`, `trimMode` and `language`, the
last of which the decoder's own model still drops.

Re-run it with:

```bash
cargo xtask roundtrip
```
