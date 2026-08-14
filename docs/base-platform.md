# Base sensor platform — design notes

Tracking: **#11** (epic) with sub-issues #12–#17.

The firmware started as a single-purpose bird-feeder scale. It is now one
configurable base for several ESP32-C3 nodes, each carrying a different subset
of sensors, selected with `NODE=<name>` at build time.

## Fleet

| `NODE=` | Node | Sensors | Power profile |
| --- | --- | --- | --- |
| `draussen` (default) | Draußen | load cell (HX711) + DS18B20 + SHT31-D | battery, deep-sleep |
| `schlafzimmer` | Schlafzimmer | SCD41 | mains, always-on |
| `wohnzimmer` | Wohnzimmer (+ kitchen zone) | SCD41 | mains |
| `kueche` | Küche | SDS011 | mains (fan) |
| `bad` | Bad | SHT31-D | mains |

## Sensor set

| Sensor | Bus | Emits | Status |
| --- | --- | --- | --- |
| HX711 load cell | bit-bang | force → grams | done (`hx711.rs`) |
| DS18B20 | 1-Wire | temperature | done (`ds18b20.rs`) |
| SHT31-D | I²C 0x44 | temperature, humidity | done (`sensors/sht31.rs`, #13) |
| SCD41 | I²C 0x62 | CO₂, temperature, humidity | done (`sensors/scd41.rs`, #14) |
| SDS011 | UART 9600 | PM2.5, PM10 | done (`sensors/sds011.rs`, #15) |

I²C addresses do not clash, so SHT31-D + SCD41 share one bus. SDS011 is the only
UART sensor. HX711 stays a blocking bit-bang critical section.

> **Bench status:** every driver is implemented and the whole fleet builds, but
> the I²C/UART drivers have not yet been run against real hardware — the bus
> timing and the SDS011 warm-up in particular want a session on the bench.

## Abstraction (#12)

`sensors::Sensor` is HAL-agnostic:

- `measure() -> Vec<Reading>` — one measurement; empty on absence (never fatal).
- `descriptors() -> &[EntityDescriptor]` — per-reading metadata for HA discovery.

`Reading` carries a `key` (topic suffix) + a pre-formatted, float-free `value`,
matching the existing on-device formatting. Shared helpers live in
`sensors/mod.rs`: `crc8_sensirion` + `crc_word` (SHT31/SCD41), `write_tenths`,
`write_int`.

The drivers are generic over the `embedded-hal-async` (I²C) and
`embedded-io-async` (UART) bus traits, so they name no esp-hal type.
`platform.rs` is the one place that does: it brings up only the buses this node
needs, wraps the single I²C peripheral in a `SharedI2c` handle (a
`NoopRawMutex` — everything is sequential on one executor) so both I²C drivers
can *own* their bus as the trait wants, and exposes one `measure_all()`.

`node.rs` holds the per-node table: identity (id/name/namespace), the sensor
slots, the power profile and the mains sampling cadence. A slot can rename its
readings (`prefix` / `label`) so two sensors emitting the same quantity don't
collide — the outdoor node's DS18B20 keeps `temperature` while its SHT31-D
publishes `air_temperature` / `air_humidity`. An unknown `NODE` value fails the
build in const-eval.

### Which node am I? (build-time default + NVS override)

The identity is resolved once at boot, before any peripheral is touched, since
the sensor set decides which buses come up at all:

1. `BUILT_AS` — the `NODE=` the image was compiled with (default `draussen`);
2. a name stored in flash, which overrides it.

`node::active()` returns the result. It is a plain `Copy` struct read from a
`static`, written once by `node::init()` — the same single-context discipline
`state.rs` uses for its RTC-RAM cells, which is sound here because nothing runs
before `init()` and nothing writes after it.

The override is set over MQTT on `smarthome/provision/<mac>` (retained), keyed
by MAC because that is the only name an unprovisioned board is sure of. A valid,
*different* name is written to flash and the board restarts into it; anything
else — an unknown name, or the name it already answers to — is ignored without
touching flash, which matters because a retained message is re-delivered on
every single connect.

The identity blob lives in the **second** NVS sector (`0xA000`), separate from
the calibration blob at `0x9000`: provisioning must never be able to disturb a
scale's tare, and the two have completely different write frequencies. Same
discipline as the config blob — magic, version, length, CRC-32 — so a blank,
erased, corrupt or half-written sector reads back as "no override" and the board
falls back to what it was built as.

The publish path iterates whatever the node produced (`Samples`) instead of
hard-coded HX711/DS18B20 calls. The HX711 and DS18B20 keep their own read paths
— the first because its reading drives the tare baseline and presence edge, the
second because its 750 ms conversion is only worth spending on publish cycles —
and contribute their readings and discovery descriptors to the same pipeline.

## MQTT auto-discovery (#16)

On the first connect after a power-up, each node publishes one retained
`homeassistant/sensor/<node>/<key>/config` per reading, so Home Assistant creates
the device (identifiers = node id) and all its entities. State topics are
`<namespace>/<node>/<prefix><key>`; the config/command topics stay
`<namespace>/<node>/config/<key>`.

"Once per power cycle" is a flag in RTC RAM: the messages are retained, so
re-sending them on every deep-sleep wake would only cost battery, while a cold
power-on (or a reflash) re-announces exactly when the broker may have lost them.

The bird scale keeps its historical `birds/scale/…` namespace, and its weight is
still mirrored to the pre-discovery `birds/scale/state` topic, so the migration
of the hand-declared Home Assistant entities can happen at leisure.

### Command entities

The knobs that flow the other way — the ones Home Assistant publishes to
`<namespace>/<node>/config/<key>` — are discovered as well, as `number`,
`switch` and `button` entities under the device's *Configuration* category. They
used to be the one thing still hand-declared in YAML, which meant each node in
the fleet needed its own copy-pasted block.

Which knobs a node gets follows from what it *is*: the calibration ones only on
a node with a load cell, the sleep/interval ones only on a battery node, since a
mains node samples on its build-time cadence and never sleeps. A dead control on
a device card is worse than a missing one.

Two details that are not obvious:

- **The button consumes its own message.** A button's payload is a constant, so
  the firmware cannot tell one press from the next; and the message must be
  retained, because a battery node is asleep when the button is pressed. So the
  node deletes the retained message (empty retained payload) once it has
  re-zeroed. The older timestamp-token scheme still works and is still
  remembered, which doubles as a backstop if a delete is ever lost.
- **`stat_t` points back at the command topic.** The node never echoes its
  stored config, so the sliders would otherwise sit at "unknown" until moved.
  Since commands are retained, the command topic already holds the last value
  set — which is exactly the state an optimistic control would show, except this
  one survives a Home Assistant restart.

### Availability

Two mechanisms, because neither alone covers a fleet that is half asleep:

| | Mains node | Battery node |
| --- | --- | --- |
| Last will (`avty_t` → `<ns>/<node>/status`) | yes | **no** |
| `expire_after` | 3 × `sample_secs` | 3 × `heartbeat_secs` |

A last will catches the node that dies *while connected*: the broker publishes
retained `offline` and Home Assistant greys the entities out at once. The node
publishes retained `online` right after each connect, and — importantly — sends
a proper MQTT `DISCONNECT` when a round is done, which tells the broker to
discard the will. Without that, a mains node that reconnects per round would be
declared dead after every single publish.

Battery nodes get **no** will at all: they are legitimately disconnected almost
all the time, so a will would mark them offline seconds after every reading.
They rely on `expire_after` instead, which is also what catches a mains node
that dies *between* rounds (a clean disconnect discards the will, so nothing
else would notice). Three missed rounds is the threshold — one miss is a lost
packet or a failed join, three is a node that stopped — with a 120 s floor.

`expire_after` is derived from the live config, so changing the heartbeat
interval clears the "discovery published" flag and re-announces the entities
with the new expiry.

## Power profiles (#17)

- **battery**: deep-sleep between samples (the bird-scale behaviour).
- **mains**: stay awake with Wi-Fi up, sample + publish every
  `NodeConfig::sample_secs` (60 s indoors, 900 s in the kitchen so the SDS011
  fan is only spun 4×/h). Continuous operation is required for CO₂ value and for
  the fan.

The profile is a per-node build-time choice. The live `config/deep_sleep` switch
still works, but only on a battery node — a mains node has nothing to gain from
sleeping and CO₂/PM continuity to lose, so it ignores it. The SCD41 follows the
profile too: periodic measurement on mains (what its self-calibration expects),
single-shot on battery.

## Known gaps

- Provisioning needs the board to reach the broker, so a node with the wrong
  Wi-Fi credentials still needs a reflash. Credentials remain build-time.
- The SDS011 warm-up (20 s) is a fixed constant rather than adaptive.
