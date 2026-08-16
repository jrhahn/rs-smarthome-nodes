# rs-smarthome-nodes

Async `no_std` Rust ([Embassy](https://embassy.dev)) firmware for a fleet of
**Seeed Studio XIAO ESP32-C3** smart-home sensor nodes. One image serves every
node: `NODE=<name>` at build time selects which sensors are populated, what the
node is called, and whether it runs on battery (deep sleep) or mains (always
on). Home Assistant picks the nodes up automatically over **MQTT
auto-discovery** — no hand-declared entities.

It started as a battery bird-feeder scale, which is still the default node
(`NODE=draussen`): on each wake-up it reads a load cell via an HX711 amplifier
and compares it against a tare baseline kept in RTC RAM. While the feeder is
empty it drops back into deep sleep for a couple of seconds — no radio — so it
catches short visits cheaply. Only when weight crosses a threshold does it bring
up Wi-Fi and publish (grams converted on-device), sampling faster while the bird
is present. A periodic **heartbeat** (default every 10 min) publishes anyway, so
Home Assistant always has a fresh reading. While online it also pulls any
retained calibration/tuning back from Home Assistant and persists it to flash.

```
┌──────────┐  bit-bang   ┌────────┐    Wi-Fi/MQTT (grams, °C)   ┌────────────────┐
│  Load    │────────────▶│ HX711  │──▶ ESP32-C3 ───────────────▶│ Home Assistant │
│  Cell    │  DT / SCK   │ 24-bit │       birds/scale/weight    │                │
└──────────┘             └────────┘   ◀── config/* (retained) ──│  (calibration) │
   DS18B20 ─ 1-Wire ─────────────────▶     grams + tuning       └────────────────┘
   SHT31-D / SCD41 ─ I²C ────────────▶  homeassistant/… (discovery, retained)
   SDS011 ─ UART ────────────────────▶
```

## The fleet

Pick a node with `NODE=` at build time (`src/node.rs`); an unknown name fails
the build rather than flashing the wrong personality onto a board. A board can
also be **provisioned** to another identity afterwards, without a rebuild — see
[below](#provisioning-a-board).

| `NODE=` | Room | Sensors | Power |
| --- | --- | --- | --- |
| `draussen` (default) | Draußen | HX711 load cell + DS18B20 + SHT31-D | battery, deep sleep |
| `schlafzimmer` | Schlafzimmer | SCD41 | mains |
| `wohnzimmer` | Wohnzimmer | SCD41 | mains |
| `kueche` | Küche | SDS011 | mains (fan) |
| `bad` | Bad | SHT31-D | mains |

### Provisioning a board

The `NODE=` value is only the identity a board *starts* with. To repurpose one —
or to flash the whole fleet with a single image and sort out which is which
afterwards — publish its node name **retained** to the board's provisioning
topic, which is keyed by MAC (the one name a board knows before it knows
anything else) and printed on every boot:

```
node 'scale' (Draußen) booted, battery profile
provision topic: smarthome/provision/a1b2c3d4e5f6
```

```bash
# Tell that board it is the kitchen node
mosquitto_pub -h <broker-ip> -r -t smarthome/provision/a1b2c3d4e5f6 -m kueche

# …and back to whatever it was flashed as
mosquitto_pub -h <broker-ip> -r -t smarthome/provision/a1b2c3d4e5f6 -m default
```

The node picks the message up the next time it is online for a publish, stores
the name in flash and restarts into it — the sensor set decides which buses come
up, so becoming a different node means a reboot. The message being retained is
the point: a battery node that is asleep gets it whenever it next wakes. It is
re-delivered on every connect, so the firmware only writes flash when the value
actually changes, and an unknown name is logged and ignored rather than obeyed.

The identity lives in its own flash sector, separate from the calibration blob —
provisioning a board never disturbs its tare or scale factor.

**Power profiles** decide the loop: *battery* nodes cold-boot out of deep sleep,
measure, publish only when there is something to say, and sleep again. *Mains*
nodes stay associated and sample on a fixed per-node cadence — CO₂ continuity
and the SDS011's duty-cycled fan both rule out deep sleep. On a battery node the
`config/deep_sleep` switch can still hold it awake for bench testing.

## Hardware

| Component        | Detail                                             |
| ---------------- | -------------------------------------------------- |
| MCU              | Seeed XIAO ESP32-C3 (RISC-V, external Wi-Fi ant.)  |
| Battery          | 2000 mAh LiPo via the XIAO's onboard charger       |
| Sensor           | 1 kg straight-bar load cell (tension S-config)     |
| Amplifier        | HX711 24-bit ADC                                   |
| Temperature      | DS18B20 waterproof 1-Wire probe (stainless steel) |
| T / RH           | SHT31-D breakout, I²C `0x44`                       |
| CO₂ / T / RH     | SCD41 breakout, I²C `0x62`                         |
| PM2.5 / PM10     | SDS011, UART 9600 8N1, **5 V supply** (fan)        |

### Pin map (XIAO ESP32-C3 silkscreen → GPIO)

| Pad | GPIO | Use |
| --- | --- | --- |
| D0  | 2  | HX711 SCK |
| D1  | 3  | HX711 DT |
| D2  | 4  | DS18B20 1-Wire (4.7 kΩ pull-up to 3V3) |
| D3  | 5  | SDS011 UART RX ← sensor TX |
| D4  | 6  | I²C SDA (SHT31-D + SCD41) |
| D5  | 7  | I²C SCL |
| D10 | 10 | SDS011 UART TX → sensor RX |

The two I²C sensors share one bus (their addresses do not clash). The SDS011
deliberately avoids D6/D7 (GPIO21/20), which are the console UART pads the log
output uses.

### Wiring (load cell → HX711)

The four load-cell leads go to the HX711's **input** side. Colours follow the
common straight-bar convention — verify against your cell's datasheet, as they
do vary.

| Load-cell lead | HX711 pin | Meaning        |
| -------------- | --------- | -------------- |
| Red            | E+        | excitation +   |
| Black          | E−        | excitation −   |
| Green          | A+        | signal +       |
| White          | A−        | signal −       |

> If loading the cell makes the reading go *down* instead of up, swap A+/A− (or
> flip the comparison in firmware — the threshold itself is the runtime
> `threshold` config value, see below).

### Wiring (HX711 → XIAO ESP32-C3)

On the XIAO ESP32-C3 the silkscreen pads map **D0 = GPIO2, D1 = GPIO3,
D2 = GPIO4** (GPIO0/GPIO1 are *not* broken out on this board).

| HX711 pin | ESP32-C3 pin | Direction |
| --------- | ------------ | --------- |
| VCC       | 3V3          | —         |
| GND       | GND          | —         |
| SCK (clk) | GPIO2 / D0   | output    |
| DT (data) | GPIO3 / D1   | input     |

### Wiring (DS18B20 → XIAO ESP32-C3)

The probe's three leads are the usual DS18B20 colours. The data line is an
**open-drain 1-Wire bus** and needs a **4.7 kΩ pull-up from DATA to 3V3**
(the MCU's weak internal pull-up is enabled as a backup, but the external one
is required for a reliable read over the ~1 m cable).

| DS18B20 lead   | ESP32-C3 pin | Direction |
| -------------- | ------------ | --------- |
| Red (VCC)      | 3V3          | —         |
| Black (GND)    | GND          | —         |
| Yellow (DATA)  | GPIO4 / D2   | 1-Wire (+ 4.7 kΩ to 3V3) |

The temperature is read whenever a weight reading is being published — on a bird
visit, and on the periodic **heartbeat** (see below) — so the ~750 ms conversion
never runs on the low-power idle-poll cycles. It is published to
`birds/scale/temperature` in °C.

## Firmware architecture

| Concern            | Implementation                                                    |
| ------------------ | ----------------------------------------------------------------- |
| HAL / async runtime| `esp-hal` 0.22 + `esp-hal-embassy`, executor driven by **TIMG0**  |
| Node selection     | [`src/node.rs`](src/node.rs) — sensor set, identity, topics and power profile per node, chosen by `NODE=` at build time |
| Sensor abstraction | [`src/sensors/`](src/sensors) — HAL-agnostic `Sensor` trait (`measure()` + `descriptors()`), drivers generic over `embedded-hal-async` / `embedded-io-async` |
| Board wiring       | [`src/platform.rs`](src/platform.rs) — concrete buses; one shared I²C handle so both I²C drivers can own their bus |
| Load-cell driver   | [`src/hx711.rs`](src/hx711.rs) — async `wait_ready()` with timeout, blocking 24+N clock read, two's-complement sign-extend to `i32` |
| Temperature driver | [`src/ds18b20.rs`](src/ds18b20.rs) — bit-bang 1-Wire on an open-drain pin, blocking time slots, async 750 ms conversion wait, CRC-checked scratchpad |
| SHT31-D / SCD41    | [`sht31.rs`](src/sensors/sht31.rs) / [`scd41.rs`](src/sensors/scd41.rs) — single-shot and periodic I²C reads, every word CRC-checked (Sensirion CRC-8), fixed-point conversions |
| SDS011             | [`sds011.rs`](src/sensors/sds011.rs) — 10-byte UART frames with checksum + resync, fan woken only for the measurement and parked again on every exit path, warm-up ended by the readings settling rather than by a fixed wait |
| HA discovery       | [`src/discovery.rs`](src/discovery.rs) — retained `homeassistant/sensor/<node>/<key>/config` per reading, all entities grouped under one device |
| Presence / tare    | [`src/state.rs`](src/state.rs) — baseline + presence edge in RTC-persistent RAM, threshold + drift tracking in `main` |
| Config / calibration | [`src/config.rs`](src/config.rs) — calibration + tuning in a CRC-guarded flash blob (`esp-storage`), loaded at boot, updated from retained MQTT while online |
| Wi-Fi + TCP/IP     | `esp-wifi` (STA + DHCP) + `embassy-net`, background `net_task`     |
| MQTT               | `rust-mqtt` (embedded-async, MQTT v5) over an `embassy-net` socket |
| Power management   | `esp_hal::rtc_cntl` RTC-timer deep sleep; short idle poll, longer active poll while a bird is present |

The HX711 read cycle is deliberately a short **blocking** critical section:
the datasheet forbids a single clock-high pulse longer than 60 µs (it would put
the chip into power-down), so the tight loop must not yield to the executor.
Waiting *for* a conversion, by contrast, is fully async so Wi-Fi and timers keep
running.

## Toolchain

The XIAO ESP32-C3 is RISC-V, so it uses the standard bare-metal target — no
Xtensa/`espup` toolchain required.

```bash
rustup toolchain install 1.83.0
rustup target add riscv32imc-unknown-none-elf --toolchain 1.83.0
cargo install espflash          # for flashing/monitoring over USB
```

> **Toolchain pin:** [`rust-toolchain.toml`](rust-toolchain.toml) pins Rust to
> **1.83.0**. Rust ≥ 1.84 correctly makes `c_char` unsigned on RISC-V, which is
> incompatible with the pre-generated C bindings in `esp-wifi` 0.11. Newer
> `esp-hal` (1.x) lifts this, but the issue targets the 0.22 line.

## Build & flash

Credentials **and** the broker address are baked in at compile time from env
vars (see [`.env.example`](.env.example)): `SSID`, `PASSWORD`, optional
`MQTT_USER` / `MQTT_PASSWORD`, and `MQTT_BROKER` (a private LAN IP, not a
secret, kept out of source). `MQTT_PORT` is still a constant in
[`src/main.rs`](src/main.rs).

The Wi-Fi credentials are only the *default*: a board can be told different ones
over its serial console and will remember them — see
[below](#wi-fi-credentials-without-a-rebuild).

```bash
# Build only (defaults to NODE=draussen, the bird scale)
cargo build --release

# Flash + serial monitor over the XIAO's USB-C (device on /dev/ttyACM0)
SSID="MyNetwork" PASSWORD="s3cret" cargo run --release

# Flash one of the other nodes
NODE=kueche SSID="MyNetwork" PASSWORD="s3cret" cargo run --release
```

Point it at your broker by setting `MQTT_BROKER` in `.env` (or on the command
line); adjust `MQTT_PORT` in `src/main.rs` if it isn't the default 1883.

📖 **Full build/flash walkthrough — including flashing with a Raspberry Pi Pico
probe — is in [FLASHING.md](FLASHING.md).**

🔌 **Complete wiring for each node — which sensor goes on which pad, per
`NODE=`, with the pull-ups and supply notes — is in
[docs/wiring.md](docs/wiring.md).** The pin map above is the summary; that page
is what you build from.

## Wi-Fi credentials without a rebuild

A board that cannot join the network cannot be told anything over the network,
so the way in is the serial console. On a **cold boot** (a power-up or a
reflash — not a deep-sleep wake) the firmware listens briefly:

```
wifi: 'your-ssid' (built in)
wifi: no usable credentials; waiting for the console
wifi provisioning: ssid <name> / psk <passphrase> / save / clear / show / done
```

Type into the same serial monitor you are watching the logs in:

```
ssid MyNetwork
psk correct horse battery staple
save
```

The board stores them in its own flash sector — separate from the calibration
and the node identity — and restarts into them. `clear` forgets them and returns
to the credentials the image was built with; `show` reports the SSID and the
passphrase's length (never its content); `done` carries on booting.

The window is 3 s when the board already has usable credentials, and 2 minutes
when it has none, since then it has nothing else to be doing. A passphrase may
contain spaces. If stored credentials are refused three times in a row, the
board falls back to the build-time pair for the rest of that run, so a typo
cannot strand it permanently.

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

## Home Assistant integration

Each node **announces itself**: on the first connect after a power-up it
publishes one retained config message per reading to
`homeassistant/sensor/<node>/<key>/config`, so Home Assistant creates the device
and its entities without any YAML. Values are ready to use — grams, °C, %, ppm,
µg/m³ — with no template maths.

| Node | State topics |
| --- | --- |
| `draussen` | `birds/scale/weight`, `birds/scale/temperature` (DS18B20), `birds/scale/air_temperature`, `birds/scale/air_humidity` (SHT31-D) |
| `schlafzimmer` / `wohnzimmer` | `smarthome/<node>/co2`, `/temperature`, `/humidity` |
| `kueche` | `smarthome/kueche/pm25`, `/pm10` |
| `bad` | `smarthome/bad/temperature`, `/humidity` |

The weight is also mirrored to the pre-discovery `birds/scale/state` topic so an
existing hand-declared entity keeps working during the migration.

**Availability.** A mains node registers an MQTT last will, so the broker
publishes retained `offline` to `<namespace>/<node>/status` the moment its
connection breaks (and the node publishes `online` on connect, disconnecting
cleanly at the end of a round so a normal publish is never mistaken for a
death). Battery nodes get no will — they are supposed to be offline between
readings — so every node also carries `expire_after` in its discovery config:
three missed publish rounds and Home Assistant invalidates the values.

The tuning/calibration knobs are *commands* rather than readings, but they are
discovered too — as `number`, `switch` and `button` entities in the device's
*Configuration* section — so a node needs no hand-written Home Assistant YAML at
all. See [`home-assistant/README.md`](home-assistant/README.md).

### Configure & calibrate from Home Assistant

Calibration (`offset`, `scale_factor`) and tuning (`threshold`, poll intervals)
are **stored on the controller in flash** and set from Home Assistant — no
reflashing. HA publishes each value **retained** to
`<namespace>/<node>/config/<key>` (`birds/scale/config/<key>` for the scale);
the firmware reads them the next time it is online for a publish (while a bird is
on / has just left the scale) and persists them. Changes therefore apply with a
**short delay**, not instantly.

| HA entity → topic (`birds/scale/config/…`) | Meaning | Node |
| ------------------------------------------ | ------- | ---- |
| `offset`            | raw HX711 value at 0 g (tare zero) | with load cell |
| `scale_factor`      | raw ticks per gram | with load cell |
| `threshold`         | grams that count as "a bird landed" | with load cell |
| `tare` (button)     | re-zero: adopts the current empty baseline as `offset` | with load cell |
| `idle_interval`     | deep-sleep seconds while empty | battery |
| `active_interval`   | deep-sleep seconds while a bird is present | battery |
| `heartbeat_interval`| seconds between periodic temp + weight publishes with no visitor (default 600) | battery |
| `deep_sleep` (switch)  | `0` = stay awake with Wi-Fi up (bench testing on USB), `1` = normal battery deep sleep | battery |

Only the knobs that do something on a given node are announced: a mains node
samples on its build-time cadence and never sleeps, so it gets none of them.
Pressing **Tarieren** publishes a retained press (the node may be asleep) which
the firmware deletes once it has re-zeroed, so it is never replayed as a second
tare.

On a blank flash the firmware falls back to built-in defaults
(`src/config.rs` — `offset` mid-scale, `scale_factor` 420, `threshold` 10 g,
2 s / 10 s idle/active intervals, 600 s heartbeat).

**Calibrating `scale_factor`:**
1. **Tare** with the pan empty (sets `offset`). The empty-pan raw value is also
   visible in the serial monitor (`HX711 raw reading: N`).
2. Place a known mass `m` and read the published grams / raw value.
3. `scale_factor = (raw_loaded − offset) / m`; enter it in the *Kalibrierfaktor*
   number. Re-check and adjust until the reading matches.

## License

MIT — see [LICENSE](LICENSE).
