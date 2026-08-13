# Base sensor platform — design notes

Tracking: **#11** (epic) with sub-issues #12–#17.

The firmware started as a single-purpose bird-feeder scale. This is the plan to
turn it into one configurable base for several ESP32-C3 nodes, each carrying a
different subset of sensors.

## Fleet

| Node | Sensors | Power profile |
| --- | --- | --- |
| Draußen | load cell (HX711) + DS18B20 + SHT31-D | battery, deep-sleep |
| Schlafzimmer | SCD41 | mains, always-on |
| Wohnzimmer (+ kitchen zone) | SCD41 | mains |
| Küche | SDS011 | mains (fan) |
| Bad | SHT31-D | mains or battery |

## Sensor set

| Sensor | Bus | Emits | Status |
| --- | --- | --- | --- |
| HX711 load cell | bit-bang | force → grams | done (`hx711.rs`) |
| DS18B20 | 1-Wire | temperature | done (`ds18b20.rs`) |
| SHT31-D | I²C 0x44 | temperature, humidity | scaffold (`sensors/sht31.rs`, #13) |
| SCD41 | I²C 0x62 | CO₂, temperature, humidity | scaffold (`sensors/scd41.rs`, #14) |
| SDS011 | UART 9600 | PM2.5, PM10 | scaffold (`sensors/sds011.rs`, #15) |

I²C addresses do not clash, so SHT31-D + SCD41 can share one bus. SDS011 is the
only UART sensor. HX711 stays a blocking bit-bang critical section.

## Abstraction (#12)

`sensors::Sensor` is HAL-agnostic:

- `measure() -> Vec<Reading>` — one measurement; empty on absence (never fatal).
- `descriptors() -> &[EntityDescriptor]` — per-reading metadata for HA discovery.

`Reading` carries a `key` (topic suffix) + a pre-formatted, float-free `value`,
matching the existing on-device formatting. Shared helpers live in
`sensors/mod.rs`: `crc8_sensirion` (SHT31/SCD41), `write_tenths`, `write_int`.

Each node selects which sensors are enabled + its identity (name/room, MQTT
topic namespace). Start with cargo features / build-time config; move to NVS
later. The publish path iterates the enabled sensors instead of the current
hard-coded HX711 + DS18B20 calls; the bird-scale weight/presence logic becomes
one sensor's specialisation.

## MQTT auto-discovery (#16)

Publish retained `homeassistant/<component>/<node>/<key>/config` per reading so
Home Assistant creates the device + entities automatically — no more
hand-declaring entities in the home-server nix. State topics:
`<namespace>/<node>/<key>`. Group a node's entities under one HA device
(identifiers = node id). Migrate the existing bird scale onto discovery.

## Power profiles (#17)

- **battery**: deep-sleep between samples (current bird-scale behaviour).
- **mains**: stay awake / modem-light-sleep, sample+publish periodically;
  continuous is required for CO₂/PM value and for the SDS011 fan.

The live `birds/scale/config/deep_sleep` switch already toggles this at runtime;
the per-node default is a build/flash-time choice.

## Scaffolding status

`sensors/` contains the trait, shared helpers, and one stub per new sensor with
the datasheet constants, conversion math (compile-time checked at the endpoints)
and discovery descriptors in place. The bus I/O in each `measure()` is a
`todo!()` to fill in with hardware on the bench. `main.rs` still runs the
untouched bird-scale path — the scaffold only compiles alongside it.
