# Building & flashing `rs-bird-scale`

This guide covers preparing the firmware and getting it onto a Seeed XIAO
ESP32-C3 — first the simple native-USB path, then how to use a **Raspberry Pi
Pico as a debug/flash probe ("picoprobe")**.

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

## 4. Flash over the XIAO's native USB-C (recommended)

The ESP32-C3 has a **built-in USB Serial/JTAG controller**, so the XIAO's USB-C
port is all you need — plug it straight into your computer.

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

## 5. Flash with a Raspberry Pi Pico ("picoprobe")

Use this when you don't want to hang the target off your PC's USB directly (e.g.
the board is potted in the feeder enclosure, or you want galvanic separation).

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
HX711 raw reading: 8402193
Wi-Fi link up, waiting for DHCP...
Got IP: 192.168.1.42/24
Published 8402193 to birds/scale/state
Entering deep sleep for 300s
```

Subscribe on the broker side to confirm the payload:

```bash
mosquitto_sub -h <broker-ip> -t 'birds/scale/state' -v
```

Then wire up the Home Assistant sensor from
[`home-assistant/configuration.yaml`](home-assistant/configuration.yaml) and
calibrate (see the [README](README.md#home-assistant-integration)).
