# Building & flashing `rs-bird-scale`

Preparing the firmware and getting it onto a Seeed XIAO ESP32-C3.

**TL;DR — you flash by just plugging the XIAO into USB-C.** The ESP32-C3 has a
built-in USB Serial/JTAG controller, so no external programmer or probe is
needed. See [§4](#4-flash-over-the-xiaos-native-usb-c-the-normal-way).

The Raspberry Pi Pico probe in [§5](#5-optional-flash-with-a-raspberry-pi-pico-picoprobe)
is **entirely optional** — only for when the native USB isn't usable (board
sealed in the enclosure, galvanic isolation, USB-JTAG disabled).

---

## 1. Prerequisites (one-time)

```bash
# Toolchain (pinned by rust-toolchain.toml, but install it explicitly once)
rustup toolchain install 1.83.0
rustup target add riscv32imc-unknown-none-elf --toolchain 1.83.0

# Flasher / monitor. espflash speaks the ESP ROM bootloader over any serial port.
cargo install espflash
```

> The XIAO ESP32-C3 is RISC-V, so it uses the stock bare-metal target —
> **no Xtensa `espup` toolchain is needed.**

## 2. Configure the build

Two things are baked in at compile time:

| Setting            | Where                                   |
| ------------------ | --------------------------------------- |
| Wi-Fi SSID / pass  | `SSID` / `PASSWORD` env vars at build   |
| MQTT broker IP/port| `MQTT_BROKER` / `MQTT_PORT` in `src/main.rs` |

Edit `MQTT_BROKER` in [`src/main.rs`](src/main.rs) to your broker's LAN address,
then pass credentials on the command line (below).

## 3. Compile

```bash
# Just build the ELF
cargo build --release
# -> target/riscv32imc-unknown-none-elf/release/rs-bird-scale
```

`espflash` reads that ELF directly; there's no separate objcopy/bin step.

---

## 4. Flash over the XIAO's native USB-C (the normal way)

**This is all you need.** The ESP32-C3 has a **built-in USB Serial/JTAG
controller**, so the XIAO's USB-C port is the programmer — plug it straight into
your computer, no probe or adapter involved.

```bash
# Build, flash, and open the serial monitor in one step
# (`cargo run` uses the `espflash flash --monitor` runner from .cargo/config.toml)
SSID="MyNetwork" PASSWORD="s3cret" cargo run --release

# …or flash an already-built ELF explicitly:
espflash flash --monitor \
  target/riscv32imc-unknown-none-elf/release/rs-bird-scale
```

Useful checks:

```bash
espflash board-info          # confirm the chip is detected
```

> **Getting into download mode:** this firmware enters deep sleep a few seconds
> after boot, which can interrupt a flash. If `espflash` can't sync, force the
> ROM bootloader: **hold the `B` (BOOT / GPIO9) button, tap `R` (RESET), release
> `B`.** Then re-run the flash command.

---

## 5. (Optional) Flash with a Raspberry Pi Pico ("picoprobe")

> **You almost certainly don't need this.** Use [§4](#4-flash-over-the-xiaos-native-usb-c-the-normal-way)
> unless the XIAO's own USB-C is unavailable — e.g. the board is potted in the
> feeder enclosure, or you want galvanic separation from your PC.

### 5a. Turn the Pico into a probe

Flash the Pico with the official **debugprobe** firmware (hold BOOTSEL, drag the
`debugprobe_on_pico.uf2` from
<https://github.com/raspberrypi/debugprobe/releases> onto the `RPI-RP2` drive).
`debugprobe` exposes two USB interfaces: a CMSIS-DAP debug port **and a
USB-to-UART bridge**.

### 5b. Which method — UART, not JTAG

The ESP32-C3 is a **RISC-V JTAG** target, but the debugprobe's CMSIS-DAP port is
SWD-oriented, and routing the C3's JTAG to external pins (GPIO4-7) requires
**irreversibly burning the `DIS_USB_JTAG` eFuse**. So the practical picoprobe
path is the **UART bridge** driving the ESP32-C3 ROM serial bootloader — no
eFuse burning, fully reversible.

> For interactive *debugging* (breakpoints via `probe-rs`), skip the Pico and
> use the XIAO's own built-in USB-JTAG over its USB-C port:
> `probe-rs run --chip esp32c3 <elf>`.

### 5c. Wiring (Pico debugprobe UART → XIAO ESP32-C3)

| Pico (debugprobe) | XIAO ESP32-C3        | Direction        |
| ----------------- | -------------------- | ---------------- |
| `GP4` (UART TX)   | `D7` / GPIO20 (U0RXD)| probe → target   |
| `GP5` (UART RX)   | `D6` / GPIO21 (U0TXD)| target → probe   |
| `GND`             | `GND`                | common ground    |

Power the XIAO from its own USB-C or the LiPo — **do not** back-feed 3V3 from the
Pico. Keep grounds common.

Because the debugprobe UART does not drive the C3's `EN`/`BOOT` lines, download
mode must be entered **manually** (hold `B`/BOOT, tap `R`/RESET, release `B`).

### 5d. Flash over the probe's serial port

The debugprobe UART shows up as a serial device (Linux: `/dev/ttyACM1`, the
second CDC interface; macOS: `/dev/cu.usbmodemXXXX`).

```bash
# 1. Put the C3 in download mode (hold B, tap R, release B).
# 2. Flash over the Pico's UART bridge:
SSID="MyNetwork" PASSWORD="s3cret" \
espflash flash --monitor \
  --port /dev/ttyACM1 \
  --baud 460800 \
  target/riscv32imc-unknown-none-elf/release/rs-bird-scale
```

If sync fails, drop the baud to `115200` (long/loose jumper wires limit rate)
and confirm TX/RX aren't swapped.

---

## 6. Verifying it works

After a successful flash the monitor should show the boot banner, a raw HX711
reading, Wi-Fi association, a DHCP address, and the MQTT publish, e.g.:

```
rs-bird-scale booted, taking a measurement
config: offset=8388608 scale=420 threshold=10g idle=2s active=10s
HX711 raw reading: 8402193
DS18B20 raw reading: 401
Wi-Fi link up, waiting for DHCP...
Got IP: 192.168.1.42/24
Published 32.3 g to birds/scale/state
Published 25.1 to birds/scale/temperature
Entering deep sleep for 10s
```

Weight is published in **grams** (converted on-device from the flash-stored
calibration); the `config:` line shows the values loaded from flash (or
built-in defaults on a blank device). Calibration and tuning are changed from
Home Assistant — see the [README](README.md#configure--calibrate-from-home-assistant).

The `DS18B20 raw reading` / temperature publish only appear on cycles where a
weight reading is sent (a bird is on, or has just left the scale); empty
idle-poll cycles skip both the radio and the temperature conversion. A
`DS18B20 not responding` line means the probe didn't answer — check the DATA
wiring and that the 4.7 kΩ pull-up to 3V3 is present.

Subscribe on the broker side to confirm the payload:

```bash
mosquitto_sub -h <broker-ip> -t 'birds/scale/state' -v
```

Then wire up the Home Assistant sensor from
[`home-assistant/configuration.yaml`](home-assistant/configuration.yaml) and
calibrate (see the [README](README.md#home-assistant-integration)).
