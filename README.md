# rs-bird-scale

Async, battery-powered **bird-feeder scale** firmware for the
**Seeed Studio XIAO ESP32-C3**, written in `no_std` Rust on the
[Embassy](https://embassy.dev) async framework.

On each wake-up the device reads a load cell via an HX711 amplifier and compares
it against a tare baseline kept in RTC RAM. While the feeder is empty it just
drops back into deep sleep for a couple of seconds — no radio — so it can catch
short visits cheaply. Only when weight crosses a threshold does it bring up
Wi-Fi and publish the weight (converted to grams on-device) to MQTT (Home
Assistant / Mosquitto), sampling faster while the bird is present. While online
it also pulls any retained calibration/tuning back from Home Assistant and
persists it to flash.

```
┌──────────┐  bit-bang   ┌────────┐    Wi-Fi/MQTT (grams, °C)   ┌────────────────┐
│  Load    │────────────▶│ HX711  │──▶ ESP32-C3 ───────────────▶│ Home Assistant │
│  Cell    │  DT / SCK   │ 24-bit │        birds/scale/state    │                │
└──────────┘             └────────┘   ◀── config/* (retained) ──│  (calibration) │
   DS18B20 ─ 1-Wire ─────────────────▶     grams + tuning       └────────────────┘
```

## Hardware

| Component        | Detail                                             |
| ---------------- | -------------------------------------------------- |
| MCU              | Seeed XIAO ESP32-C3 (RISC-V, external Wi-Fi ant.)  |
| Battery          | 2000 mAh LiPo via the XIAO's onboard charger       |
| Sensor           | 1 kg straight-bar load cell (tension S-config)     |
| Amplifier        | HX711 24-bit ADC                                   |
| Temperature      | DS18B20 waterproof 1-Wire probe (stainless steel) |

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
> flip the comparison in firmware — see `PRESENCE_THRESHOLD` in `src/main.rs`).

### Wiring (HX711 → XIAO ESP32-C3)

| HX711 pin | ESP32-C3 pin | Direction |
| --------- | ------------ | --------- |
| VCC       | 3V3          | —         |
| GND       | GND          | —         |
| DT (data) | GPIO1 / D1   | input     |
| SCK (clk) | GPIO0 / D0   | output    |

### Wiring (DS18B20 → XIAO ESP32-C3)

The probe's three leads are the usual DS18B20 colours. The data line is an
**open-drain 1-Wire bus** and needs a **4.7 kΩ pull-up from DATA to 3V3**
(the MCU's weak internal pull-up is enabled as a backup, but the external one
is required for a reliable read over the ~1 m cable).

| DS18B20 lead   | ESP32-C3 pin | Direction |
| -------------- | ------------ | --------- |
| Red (VCC)      | 3V3          | —         |
| Black (GND)    | GND          | —         |
| Yellow (DATA)  | GPIO2 / D2   | 1-Wire (+ 4.7 kΩ to 3V3) |

The temperature is read only when a weight reading is being published (a bird
is on / has just left the scale), so the ~750 ms conversion never runs on the
low-power idle-poll cycles. It is published to `birds/scale/temperature` in °C.

## Firmware architecture

| Concern            | Implementation                                                    |
| ------------------ | ----------------------------------------------------------------- |
| HAL / async runtime| `esp-hal` 0.22 + `esp-hal-embassy`, executor driven by **TIMG0**  |
| Load-cell driver   | [`src/hx711.rs`](src/hx711.rs) — async `wait_ready()` with timeout, blocking 24+N clock read, two's-complement sign-extend to `i32` |
| Temperature driver | [`src/ds18b20.rs`](src/ds18b20.rs) — bit-bang 1-Wire on an open-drain pin, blocking time slots, async 750 ms conversion wait, CRC-checked scratchpad |
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

Credentials are baked in at compile time; the broker IP is a constant in
[`src/main.rs`](src/main.rs) (`MQTT_BROKER`).

```bash
# Build only
cargo build --release

# Flash + serial monitor over the XIAO's USB-C (device on /dev/ttyACM0)
SSID="MyNetwork" PASSWORD="s3cret" cargo run --release
```

Adjust `MQTT_BROKER` / `MQTT_PORT` in `src/main.rs` to point at your broker.

📖 **Full build/flash walkthrough — including flashing with a Raspberry Pi Pico
probe — is in [FLASHING.md](FLASHING.md).**

## Home Assistant integration

The device publishes ready-to-use **grams** to `birds/scale/state` and **°C** to
`birds/scale/temperature` — no template maths needed. Add the sensor + config
entities from
[`home-assistant/configuration.yaml`](home-assistant/configuration.yaml).

### Configure & calibrate from Home Assistant

Calibration (`offset`, `scale_factor`) and tuning (`threshold`, poll intervals)
are **stored on the controller in flash** and set from Home Assistant — no
reflashing. HA publishes each value **retained** to `birds/scale/config/<key>`;
the firmware reads them the next time it is online for a publish (while a bird is
on / has just left the scale) and persists them. Changes therefore apply with a
**short delay**, not instantly.

| HA entity → topic (`birds/scale/config/…`) | Meaning |
| ------------------------------------------ | ------- |
| `offset`            | raw HX711 value at 0 g (tare zero) |
| `scale_factor`      | raw ticks per gram |
| `threshold`         | grams that count as "a bird landed" |
| `idle_interval`     | deep-sleep seconds while empty |
| `active_interval`   | deep-sleep seconds while a bird is present |
| `tare` (button/script) | re-zero: adopts the current empty baseline as `offset` |

On a blank flash the firmware falls back to built-in defaults
(`src/config.rs` — `offset` mid-scale, `scale_factor` 420, `threshold` 10 g,
2 s / 10 s intervals).

**Calibrating `scale_factor`:**
1. **Tare** with the pan empty (sets `offset`). The empty-pan raw value is also
   visible in the serial monitor (`HX711 raw reading: N`).
2. Place a known mass `m` and read the published grams / raw value.
3. `scale_factor = (raw_loaded − offset) / m`; enter it in the *Kalibrierfaktor*
   number. Re-check and adjust until the reading matches.

## License

MIT — see [LICENSE](LICENSE).
