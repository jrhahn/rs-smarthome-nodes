# rs-bird-scale

Async, battery-powered **bird-feeder scale** firmware for the
**Seeed Studio XIAO ESP32-C3**, written in `no_std` Rust on the
[Embassy](https://embassy.dev) async framework.

On each wake-up the device reads a load cell via an HX711 amplifier and compares
it against a tare baseline kept in RTC RAM. While the feeder is empty it just
drops back into deep sleep for a couple of seconds — no radio — so it can catch
short visits cheaply. Only when weight crosses a threshold does it bring up
Wi-Fi and publish the raw 24-bit value to MQTT (Home Assistant / Mosquitto),
sampling faster while the bird is present.

```
┌──────────┐  bit-bang   ┌────────┐   Wi-Fi/MQTT   ┌────────────────┐
│  Load    │────────────▶│ HX711  │──▶ ESP32-C3 ──▶│ Home Assistant │
│  Cell    │  DT / SCK   │ 24-bit │   birds/scale  │  (calibration) │
└──────────┘             └────────┘     /state     └────────────────┘
```

## Hardware

| Component        | Detail                                             |
| ---------------- | -------------------------------------------------- |
| MCU              | Seeed XIAO ESP32-C3 (RISC-V, external Wi-Fi ant.)  |
| Battery          | 2000 mAh LiPo via the XIAO's onboard charger       |
| Sensor           | 1 kg straight-bar load cell (tension S-config)     |
| Amplifier        | HX711 24-bit ADC                                   |

### Wiring (HX711 → XIAO ESP32-C3)

| HX711 pin | ESP32-C3 pin | Direction |
| --------- | ------------ | --------- |
| VCC       | 3V3          | —         |
| GND       | GND          | —         |
| DT (data) | GPIO1 / D1   | input     |
| SCK (clk) | GPIO0 / D0   | output    |

## Firmware architecture

| Concern            | Implementation                                                    |
| ------------------ | ----------------------------------------------------------------- |
| HAL / async runtime| `esp-hal` 0.22 + `esp-hal-embassy`, executor driven by **TIMG0**  |
| Load-cell driver   | [`src/hx711.rs`](src/hx711.rs) — async `wait_ready()` with timeout, blocking 24+N clock read, two's-complement sign-extend to `i32` |
| Presence / tare    | [`src/state.rs`](src/state.rs) — baseline + presence edge in RTC-persistent RAM, threshold + drift tracking in `main` |
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

Calibration math (tare offset + ticks-per-gram) lives in Home Assistant so it
can be tuned without reflashing. Add the snippet from
[`home-assistant/configuration.yaml`](home-assistant/configuration.yaml):

```yaml
sensor:
  - platform: mqtt
    name: "Meisenknödel Gewicht"
    state_topic: "birds/scale/state"
    unit_of_measurement: "g"
    value_template: >
      {% set raw = value | int %}
      {% set offset = 8388608 %}     {# raw tare offset  #}
      {% set scale_factor = 420.0 %} {# ticks per gram   #}
      {{ ((raw - offset) / scale_factor) | round(1) }}
```

**Calibrating:** watch the raw values on `birds/scale/state` with the pan empty
(that's your `offset`), then place a known mass and compute
`scale_factor = (raw_loaded − offset) / grams`.

## License

MIT — see [LICENSE](LICENSE).
