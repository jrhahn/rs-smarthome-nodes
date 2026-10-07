# Changelog

What changed in this repository, newest first. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**This is not [`docs/annotations.md`](docs/annotations.md), and the two must not
drift into each other.** This file says *what changed in the code*, one entry
per change, written when the change is made. The fleet log says *what happened
to the hardware* — a rollout, an outage, a measurement that corrected an
assumption — written when it happens. A commit that fixes a join retry belongs
here; the evening it went on `kueche` and `bad` belongs there, and the two link
to each other rather than repeating.

**The version here is not `FW_VERSION`.** A node publishes `<node>-<commit>`
(`build.rs`), which names an exact build and is what an over-the-air offer must
match. The number below names a set of changes. Wiring the two together is
worth doing and has not been done.

Entries start at 0.2.0. Earlier history is in `git log` and in the fleet log;
nothing was backfilled, because a changelog written after the fact is a
reconstruction and reads like one.

## [Unreleased]

## [0.2.0] — 2026-10-07

### Fixed

- **A refused Wi-Fi join no longer costs a whole publish round.** The join
  retried on a flat five seconds, which fits twice inside the 20 s
  `WIFI_BUDGET`, so a single refusal ended the round. It now backs off from
  500 ms to a 30 s ceiling, paced by a task-local counter so every round keeps
  its fast first retry. Measured on `kueche`: 47 of 635 rounds lost in 22 hours,
  and 42 of those refusals had cleared by the following round.
- **`reset_count` and the join backoff no longer share a counter.**
  `JOIN_REFUSALS` lives in RTC RAM so it can accumulate towards
  `FALLBACK_AFTER` across a sleeping node's rounds; pacing retries from it had
  every round after the first open already backed off.

### Added

- **A node that cannot reach the broker now sleeps on it.** A publish round that
  never reached the broker doubles the next sleep; the first round that lands
  resets it. The ceiling hangs off `PowerProfile` — one doubling on mains, four
  on a cell — and is bounded by over-the-air reach rather than by power, because
  a node that has backed off cannot be updated for that long. A battery node's
  idle poll is deliberately not stretched: it reads the load cell with the radio
  off, so backing it off would cost presence detection to save nothing.

### Notes

- Neither change addresses the **watchdog resets** (`reset_reason` 7) seen on
  both duty-cycled nodes. An expired `with_timeout` is a clean abort, not a
  stopped executor. That hang is still open.
- Rolled out to `kueche` and `bad` on 2026-10-07; see the fleet log.

[Unreleased]: https://github.com/jrhahn/rs-smarthome-nodes/compare/v0.2.0...develop
[0.2.0]: https://github.com/jrhahn/rs-smarthome-nodes/releases/tag/v0.2.0
