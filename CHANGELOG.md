# Changelog

What changed in this repository, newest first. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**This is not the fleet log, and the two must not drift into each other.** This
file says *what changed in the code*, one entry per change, written when the
change is made. The fleet log says *what happened to the hardware* — a rollout,
an outage, a measurement that corrected an assumption — written when it
happens. It lives with the deployment, in a private repository, since flavors
replaced the room table — before that it was `docs/annotations.md` here, and
older entries below still name rooms because the firmware did. A commit that
fixes a join retry belongs here; the evening it went on two boards belongs
there.

**The version here is part of `FW_VERSION`.** A node publishes
`<id>-<release>-<commit>` (`build.rs`), e.g. `kitchen-0.2.0-6c81631`: the
release is `version` from `Cargo.toml`, the commit names the exact build and is
what an over-the-air offer must match. Images built before #39 publish
`<node>-<commit>` only.

Entries start at 0.2.0. Earlier history is in `git log` and in the fleet log;
nothing was backfilled, because a changelog written after the fact is a
reconstruction and reads like one.

## [Unreleased]

### Added

- **A floor plan on the archive dashboard.** `web.floorplan` names an SVG and
  `web.rooms` maps node ids to the ids of its rooms; the dashboard then draws
  the plan above the cards, tints each room by its worst reading (or grey when
  stale) and writes the boards' lead values into it, with CO₂ always shown and a
  short name per board where a room has several. Served by the new
  `GET /api/floorplan`, which answers 404 when no plan is configured, so a
  setup without one looks as before.

### Changed

- **The solar lamp goes out at 21:30 UTC instead of 22:00** — 22:30 in
  winter, 23:30 in summer, still without a daylight-saving rule. The fade-out
  starts ten minutes earlier with it. Half an hour less a night, for the
  evenings the cell is too low to light at all. Takes effect on reflash.
- **QuestDB stops recording its own activity and polls with one network
  worker.** The module's default `server.conf` adds
  `telemetry.disable.completely=true`, because `telemetry.enabled=false` alone
  left QuestDB 9.3 writing every WAL commit into `sys.telemetry_wal`, and
  `shared.network.worker.count=1`, because the archiver's loopback connections
  are all the network I/O there is and each idle worker polls on its own.
- **QuestDB runs quieter.** The NixOS module now writes a `log.conf` next to
  `server.conf` that logs errors, critical messages and advisories only, not
  every WAL commit, apply and view refresh at INFO (~900 lines a minute and
  ~250 MB a day on a server with three rollup tiers), and that also writes
  them to stdout, so they reach the journal. Its default
  `server.conf` also sets `telemetry.enabled=false`, so QuestDB stops filling
  `sys.telemetry_*` tables and reporting usage; what those tables already
  hold stays until they are dropped. A setup that sets
  `questdb.extraConfig` itself keeps its own file and decides on telemetry
  there.
- **Boards are built from flavors, not from a table of rooms — breaking for
  every build command.** `src/node.rs` now lists five flavors (`climate`,
  `climate-co2`, `air-quality`, `bird-scale`, `solar-lamp`): a sensor set and a
  power profile each, and nothing about where a board hangs. The identity
  comes from the build: `FLAVOR=` picks the entry, `NODE_ID=` is the topic
  level, client id and unique-id prefix, `NODE_NAME=` the device name in Home
  Assistant. `NODE=` is gone and refused with a message naming its
  replacement; an unknown flavor or a malformed id fails the build. A board
  rebuilt with its old room name as `NODE_ID` publishes exactly what it did
  before — same topics, same unique ids, same `FW_VERSION` — so no series and
  no Home Assistant entity changes. Provisioning over
  `smarthome/provision/<mac>` now switches a board's flavor and keeps its id;
  room names in a provisioning sector are ignored. `meta/board` gains a
  `flavor` field.
- **The fleet log and the commissioning log left this repository.**
  `docs/annotations.md` and `docs/commissioning.md` describe one house, not
  the firmware, and moved with their history to the deployment's private
  repository.
- **Link and reset counters are announced as diagnostics.** `rssi`,
  `reset_reason`, `reset_count` and `boot_count` now carry
  `ent_cat: diagnostic` in their discovery message, so Home Assistant files
  them under the device page's diagnostics and the archive's dashboard leaves
  them out without a list of its own. Entity ids and topics are unchanged.
  Takes effect per node on reflash.
- **The firmware moved to esp-hal 1.1** (#32), and the image now carries an
  ESP-IDF app descriptor, so it flashes with **espflash 4.x**; the 3.x pin and
  the second nixpkgs input in `flake.nix` are gone. Chosen as the newest set
  with no beta crate in it: `esp-hal` 1.1.2, `esp-radio` 0.18 (was `esp-wifi`),
  `esp-rtos` 0.3 (was `esp-hal-embassy`), `esp-bootloader-esp-idf` 0.5,
  `embassy-net` 0.9. Rust moves from 1.83 to 1.95. No intended change in what a
  node does; RTC RAM may be re-laid-out, so counters held there (`reset_count`,
  the join counter, the parked flag) can read once as garbage after the update.
  rust-mqtt stays at 0.3 behind a small adapter to embedded-io-async 0.7.
- **`FW_VERSION` now carries the release: `<node>-<release>-<commit>`**, e.g.
  `kueche-0.2.0-6c81631`, with `-dirty` still last. The release is read from
  `Cargo.toml`, so `ota/version`, `meta/board` and the Home Assistant `sw`
  field say which changelog entry a board is on. OTA offers must use the new
  string; the comparison stays exact, and a node on an old-format version takes
  a new-format offer and then recognises it as running (#39).

### Fixed

- **The solar lamp's evening readings carry honest timestamps.** It synced
  the time once at boot and then stamped every five-minute round from that
  anchor plus its monotonic timer, for hours: on 2026-10-16 its readings were
  three to four minutes in the future by the evening's second hour. Each round
  now asks the time server again and re-anchors on the answer; a round whose
  sync fails goes out unstamped and is dated on arrival, as on every other
  flavor. Dusk and the cutoff still run on the carried clock. Takes effect on
  reflash.
- **A chart opened from a link shows its channel's name, not its key.** When
  the series arrived before the channel list, the title stayed `temperature`
  instead of `Temperatur` until the next refresh; it is now relabelled as soon
  as the names arrive.
- **A mistyped `NODE=` now lists `solarleuchte` among the valid names.** The
  build-time error spelled the list out a second time and had missed it; the
  message and `KNOWN_NODES` now share one literal.
- **A restart of the archiver no longer suspends the readings table** (#53).
  The service re-applied `SET TTL` and `DEDUP` on every start. Each is a
  structure change in QuestDB's WAL even when it changes nothing, and since
  the 2026-10-07 power loss the base table cannot apply those, so every
  restart or reboot stopped ingestion until the transaction was skipped by
  hand. It now reads `SHOW CREATE` first and alters only what differs.
- **The dashboard no longer reads rollup views QuestDB has invalidated** (#53).
  The service lists only `valid` views, drops any view whose source view is not
  usable, and re-checks every minute, so a broken view stops being read and a
  rebuilt one is picked up without a restart. The overview's sparklines now fall
  back to the base table when a view has nothing for the window, as the detail
  chart already did: after the 2026-10-07 power loss the overview showed "no
  data" on every tile while each chart behind it drew.

### Added

- **The dashboard opens on a room view without the debug channels.** One card
  per node, its main reading large with a sparkline and the rest compact;
  diagnostics, the firmware's `rssi` / `boot_count` / `reset_*`, unnamed
  topics, raw twins and supply voltage and current are left out. The channel
  wall is still there as **Live**. The way back from a chart moved from inside
  the card to beside the title, labelled with the view it returns to.
  CO₂ at or above 1000 ppm is drawn amber and marked *elevated*, at or above
  2000 ppm red and *high* (the Umweltbundesamt's indoor guidance).
- **The dashboard has a second view: values per day, week and month** (#52).
  Periods follow the house's calendar (`Europe/Berlin`, weeks from Monday).
  Meters — electricity in kWh, water in m³, anything announced
  `total_increasing` — show their consumption per period as bars, computed from
  the last reading of each period so a camera meter's misread cancels itself
  out; everything else shows its mean, min and max. New endpoint
  `/api/periods`; `ChannelMeta` now carries `state_class` and
  `entity_category` from the discovery messages.
- **A battery node parks on a low cell.** Below 3.0 V (`battery::LOW_CELL_MV`)
  `terrasse` stops polling the scale, reports once and sleeps 3 h between
  checks, each of which publishes. It resumes above 3.3 V, so a cell that
  recovers at rest does not flap. A reading under 2.0 V is treated as a wiring
  fault and never parks. `solarleuchte` already went dark and slept until the
  next dusk below its 20 % charge gate, and is unchanged (#37).

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
