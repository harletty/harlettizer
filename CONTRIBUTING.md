# Contributing

## Before anything else: provenance

[LEGAL.md](LEGAL.md) is normative, not advisory. Two rules decide whether a
patch can be accepted at all:

1. **Where the code came from is part of the patch.** If any of it derives
   from another project, add the row to [docs/provenance.md](docs/provenance.md)
   in the same commit — file, upstream revision, license, what changed — and
   keep the upstream copyright header in the file. GPL-compatible sources are
   listed in LEGAL.md §2.
2. **Some sources are closed.** Nothing from Cavern (its licence is not free
   and is GPL-incompatible), and nothing from a vendor SDK. A measurement
   against the proprietary encoder is admissible only from a licensed run —
   LEGAL.md §3.1.

A patch that cannot say where its code came from will be asked where its code
came from.

## Language

Everything written is in English: commit messages, PR titles and bodies, code
comments, documentation.

## Branch names

Generic and descriptive — `feat/mlp-encoder`, `fix/bw64-chna`. No format brand
or trademark in a branch name.

## Working on the code

```bash
cargo build --all-targets
cargo test --all
cargo fmt --all
```

`cargo fmt --check` is gated in CI. Clippy is advisory for now.

The test suite needs no external material.

## Performance

This code runs over whole programmes and the long-term target includes
constrained hardware. In any path that runs per sample, per block or per
frame: no allocation, no hash lookups, no recomputing what can be computed
once. When two implementations are possible, the one with the simpler
steady-state cost wins, not the one that is quicker to write.
