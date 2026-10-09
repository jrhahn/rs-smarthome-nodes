# rs-smarthome-nodes

Async `no_std` Rust ([Embassy](https://embassy.dev)) firmware for a fleet of
**Seeed Studio XIAO ESP32-C3** smart-home sensor nodes. One image serves every
node: `NODE=<name>` at build time selects which sensors are populated, what the
node is called, and whether it sleeps between readings or stays associated.
Home Assistant picks the nodes up automatically over **MQTT
auto-discovery** — no hand-declared entities.

It started as a battery bird-feeder scale, which is still the default node
(`NODE=terrasse`): on each wake-up it reads a load cell via an HX711 amplifier
and compares it against a tare baseline kept in RTC RAM. While the feeder is
empty it polls every couple of seconds from **light** sleep — no radio, and no
cold boot per poll — which bounds how short a visit it can see at all. The
amplifier is powered down between those polls, and its settling time after
power-up is waited out asleep rather than awake. Once weight crosses the threshold it
stops sleeping and **watches the visit through awake**, so the published weight
is a settled median rather than whichever conversion happened to land first, and
the visit gets a real duration instead of one rounded to the sleep interval;
then it brings up Wi-Fi and publishes once (grams converted on-device). A
periodic **heartbeat** (default every 10 min) publishes anyway, so
Home Assistant always has a fresh reading. While online it also pulls any
retained calibration/tuning back from Home Assistant and persists it to flash.

```
┌──────────┐  bit-bang   ┌────────┐       Wi-Fi/MQTT            ┌────────────────┐
│  Load    │────────────▶│ HX711  │──▶ ESP32-C3 ───────────────▶│ Home Assistant │
│  Cell    │  DT / SCK   │ 24-bit │   smarthome/<node>/<key>    │                │
└──────────┘             └────────┘   ◀── config/* (retained) ──│  (calibration) │
   SHT31-D / SCD41 / SGP41 ─ I²C ────▶                          └────────────────┘
   SDS011 ─ UART ────────────────────▶  homeassistant/… (discovery, retained)
   battery divider ─ ADC ────────────▶
```

## The fleet

Pick a node with `NODE=` at build time (`src/node.rs`); an unknown name fails
the build rather than flashing the wrong personality onto a board. A board can
also be **provisioned** to another identity afterwards, without a rebuild.

| `NODE=` | Room | Sensors | Outputs | Power |
| --- | --- | --- | --- | --- |
| `terrasse` (default) | Terrasse | HX711 load cell + SHT31-D + cell voltage | — | battery, deep sleep |
| `schlafzimmer` | Schlafzimmer | SCD41 + SHT31-D | — | mains |
| `wohnzimmer` | Wohnzimmer | SCD41 + SHT31-D + SDS011 + SGP41 | — | mains (fan) |
| `kueche` | Küche | SHT31-D | — | mains, duty-cycled |
| `bad` | Bad | SHT31-D | — | mains, duty-cycled |
| `solarleuchte` | Garten | SHT31-D + cell voltage | LED string (LEDC) | battery, deep sleep |

A board can be told to become another node over MQTT, without reflashing — see
[provisioning](docs/flashing.md#7-provisioning-a-board-without-reflashing).

**Power profiles** decide the loop. *Battery* nodes cold-boot out of deep sleep,
measure, publish only when there is something to say, and sleep again, at the
runtime intervals set from [Home Assistant](docs/home-assistant.md#configure--calibrate-from-home-assistant). *Mains* nodes stay associated and sample on a fixed
per-node cadence — CO₂ continuity and the SDS011's duty-cycled fan both rule out
deep sleep. A sensor whose own cadence is slower than the node's round says so
per slot (`Slot::every`), which is how `wohnzimmer` reads CO₂ every minute while
its fan runs four times an hour.

*Mains, duty-cycled* is the third: on a cable, but deep-sleeping between rounds
anyway. The reason is heat rather than power. A node that keeps Wi-Fi up draws
something like 80–110 mA without pause, which is roughly a third of a watt
warming the inside of a small box, and a node whose entire job is to report the
room's temperature cannot afford to warm the air it is measuring. `kueche` and
`bad` carry nothing that needs continuity, so they stop running between
readings, sleeping their own `sample_secs` — the cadence they already published
at, so no history gets a step in it. Any node that sleeps at all keeps the
`config/deep_sleep` switch, which holds it awake for bench testing.

## Quick start

```bash
cp .env.example .env              # Wi-Fi + MQTT credentials; direnv loads it
NODE=kueche cargo run --release   # build, flash over USB-C, open the monitor
```

The full procedure — toolchain, the partition table, flashing a sleeping node,
what a healthy boot log looks like — is in [docs/flashing.md](docs/flashing.md).

## Documentation

| | |
| --- | --- |
| [docs/flashing.md](docs/flashing.md) | building, flashing, provisioning, Wi-Fi credentials over the console |
| [docs/wiring.md](docs/wiring.md) | pin map and wiring for every node |
| [docs/home-assistant.md](docs/home-assistant.md) | topics, entities, calibration and tuning knobs |
| [docs/enclosure.md](docs/enclosure.md) | the printed enclosures (`cad/`) |
| [docs/base-platform.md](docs/base-platform.md) | design notes: node abstraction, power profiles, discovery, testing |
| [docs/ota.md](docs/ota.md) | over-the-air updates |
| [docs/solar.md](docs/solar.md), [docs/solarleuchte.md](docs/solarleuchte.md) | solar for `terrasse`; the rebuilt garden lamp |
| [docs/long-term-history.md](docs/long-term-history.md) | keeping the fleet's data for years |
| [docs/commissioning.md](docs/commissioning.md) | what is physically built, board by board |
| [docs/annotations.md](docs/annotations.md) | fleet log: rollouts, outages, recalibrations — dated |
| [CHANGELOG.md](CHANGELOG.md) | what changed in the code |

## Repository layout

```
src/            firmware (lib + bin); sensors/ holds the bus-generic drivers
examples/       lamp_bench — bench rig for the solarleuchte lamp
timeseries/     the QuestDB archiver and dashboard (own crate)
cad/            CadQuery source (models.py) and exported STEP/STL
docs/           everything above
partitions.csv  OTA partition table, passed on every flash
```

## Tests

A firmware binary for `riscv32imc` cannot host a test harness, so the crate is
split: [`src/lib.rs`](src/lib.rs) holds everything the binary is made of, and
the parts that are **pure computation** — frame decoding, CRCs, the flash blob
layouts, `Config::apply`, the discovery payloads, the node table — build without
the HAL and therefore run on the host.

```bash
# What CI runs
cargo test --no-default-features --features host-tests \
    --target x86_64-unknown-linux-gnu
```

The sensor drivers themselves are covered too. They are generic over the
`embedded-hal-async` / `embedded-io-async` bus traits, so
[`src/sensors/mock.rs`](src/sensors/mock.rs) can feed the *real* SHT31, SCD41
and SDS011 drivers a scripted bus: canned I²C replies, and a UART that hands
over stale frames, falls quiet, then delivers the frame that matters. That
covers the resync, the CRC rejection, the fan duty cycle, the SCD41's two run
modes and its data-ready handshake, and what happens when a sensor is absent —
but not timing, bus contention or anything electrical, which stay bench
questions.

Anything that touches the chip itself — RTC RAM, flash, the radio, the bit-bang
drivers — sits behind the `hal` feature (on by default), so a normal
`cargo build` is unaffected. Compile-time
`const _: () = assert!(…)` checks stay where they are: they cost nothing and
fail the *build*, which is stronger than a test — the tests cover what
const-eval cannot reach, such as anything that formats a string, negative cases,
and inputs enumerated in a loop.

[CI](.github/workflows/ci.yml) runs the tests, both clippy passes, and builds
every node in the fleet — plus a check that a mistyped `NODE=` still fails the
build.

## Long-term history

Home Assistant sees every node the moment it publishes, and keeps a recorder
database — tuned for weeks. The questions this fleet was built for are slower
than that: did insulating the roof change the bedroom's overnight CO₂, how does
the terrace swing between summers, is the feeder busier this year than last.

[`timeseries/`](timeseries/README.md) is a second consumer on the same broker
whose only job is not to lose anything. It follows the topics the nodes already
publish, writes each reading into **QuestDB** with a **three-year retention**,
maintains three cascading rollup views (`_1m` → `_1h` → `_1d`) so a year-wide
chart reads thousands of rows instead of millions, and serves a small dashboard
that routes each query to the coarsest view still fine enough to answer it.

```bash
nix develop .#timeseries
cd timeseries && cargo run -- timeseries.example.toml   # then http://127.0.0.1:8087
```

It changes nothing about the firmware or the Home Assistant side — a node does
not know it is being archived. The flake carries a package and a NixOS module
(which brings up QuestDB too, since nixpkgs ships the package but no service)
for the home server; see [`timeseries/README.md`](timeseries/README.md) for the
schema, the rollup reasoning, and how to deploy it.

## License

MIT — see [LICENSE](LICENSE).
