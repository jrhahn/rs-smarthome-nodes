# Working in this repository

## Branching

**Never commit to `develop`.** Work on a feature branch off it, named
`feature/<short-kebab-case>`, and open a pull request. This holds for a one-line
fix and for documentation.

`develop` is the integration branch and the one `home-server-private` pins its
flake input to, so a commit landing there is a commit the house can deploy.
That is the reason for the rule: nothing reaches it without a review step, even
a trivial change.

## Changelog

**Every change that alters behaviour gets an entry in `CHANGELOG.md` under
`## [Unreleased]`, in the same commit as the change.** Not afterwards, and not
in a batch at release time — an entry written a week later is a reconstruction.

Refactors, comment fixes and test-only changes need no entry.

To cut a version: rename `[Unreleased]` to the new number with today's date,
open a fresh empty `[Unreleased]` above it, bump `version` in `Cargo.toml`, and
update the two link definitions at the bottom of the file. Semantic versioning,
against the behaviour a node shows — a changed publish cadence or topic layout
is breaking, a new sensor channel is a minor, a fix that makes an existing
channel behave as documented is a patch.

### `CHANGELOG.md` against `docs/annotations.md`

Two files, two jobs, and letting them blur is how one of them rots.

- **`CHANGELOG.md`** — what changed in the code. One entry per change, written
  with the change.
- **`docs/annotations.md`** — what happened to the fleet. A rollout, an outage,
  a measurement that corrected an assumption. Written when it happens, dated to
  the minute, and it is the file that explains a step in the archive to whoever
  reads that data in a year.

A commit that fixes a join retry goes in the changelog. The evening it went on
`kueche` and `bad`, with the counters before and after, goes in the fleet log.
Cross-reference rather than repeat.

**A rollout that is not written down costs an evening.** The 2026-10-06 update
to `kueche` was not recorded, and finding out what had been done to that node
took most of 2026-10-07. Record it as part of doing it.

## Language

**English** for everything that outlives the conversation and everything other
people read: commit messages, pull requests, issues, code, comments,
documentation, and both files above. Issues and PRs in English without
exception, whatever language the discussion that produced them was in.

Chat may be in whatever language the person is using.

## Before committing

```bash
cargo test --no-default-features --features host-tests --target x86_64-unknown-linux-gnu
cargo clippy --no-default-features --features host-tests --target x86_64-unknown-linux-gnu --all-targets
cargo build --release          # the firmware actually links
```

`espflash` must be **4.x** since #32 (the image carries an ESP-IDF app
descriptor); an image from before #32 still needs 3.x.
