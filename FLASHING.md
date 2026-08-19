# Building & flashing `rs-smarthome-nodes`

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

Everything below is baked in at compile time (see [`.env.example`](.env.example);
`direnv` exports a local `.env` automatically):

| Setting             | Where                                          |
| ------------------- | ---------------------------------------------- |
| **Which node**      | `NODE` env var — decides the sensors, topics and power profile |
| Wi-Fi SSID / pass   | `SSID` / `PASSWORD` env vars at build          |
| MQTT broker         | `MQTT_BROKER` env var (dotted IPv4)            |
| MQTT credentials    | optional `MQTT_USER` / `MQTT_PASSWORD` env vars |
| MQTT port           | `MQTT_PORT` in [`src/main.rs`](src/main.rs)    |

`NODE` is the important one — one image serves the whole fleet:

| `NODE=` | Sensors | Power |
| --- | --- | --- |
| `draussen` (default) | HX711 + DS18B20 + SHT31-D | battery, deep sleep |
| `schlafzimmer`, `wohnzimmer` | SCD41 | mains |
| `kueche` | SDS011 | mains |
| `bad` | SHT31-D | mains |

A typo fails the build rather than flashing the wrong personality onto a board:

```
error[E0080]: evaluation of constant value failed
  = the evaluated program panicked at 'unknown NODE; expected one of:
    draussen, schlafzimmer, wohnzimmer, kueche, bad'
```

## 3. Compile

```bash
# Just build the ELF (defaults to NODE=draussen)
cargo build --release
# -> target/riscv32imc-unknown-none-elf/release/rs-smarthome-nodes

# …or build for another node
NODE=kueche cargo build --release
```

> Each node is a **separate build**, so rebuild before flashing a different
> board — the ELF path is the same for all of them. Or flash the same image
> everywhere and tell each board what it is afterwards; see
> [§7](#7-provisioning-a-board-without-reflashing).

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

# …for one of the other nodes
NODE=kueche SSID="MyNetwork" PASSWORD="s3cret" cargo run --release

# …or flash an already-built ELF explicitly:
espflash flash --monitor \
  target/riscv32imc-unknown-none-elf/release/rs-smarthome-nodes
```

Useful checks:

```bash
espflash board-info          # confirm the chip is detected
```

> **Getting into download mode:** a battery node (`NODE=draussen`) enters deep
> sleep a couple of seconds after boot, which can interrupt a flash — mains
> nodes stay awake. If `espflash` can't sync, force the
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
  target/riscv32imc-unknown-none-elf/release/rs-smarthome-nodes
```

If sync fails, drop the baud to `115200` (long/loose jumper wires limit rate)
and confirm TX/RX aren't swapped.

---

## 6. Verifying it works

The first line names the node the image was built for — check it before anything
else, it is the one mistake that produces a board that looks perfectly healthy
while publishing to the wrong room.

**Outdoor scale (`NODE=draussen`, battery)** — a publish cycle:

```
node 'scale' (Draußen) booted, battery profile
config: offset=8388608 scale=420 threshold=10g idle=2s active=10s
HX711 raw reading: 8402193
presence: raw=8402193 baseline=8388608 delta=13585
weight = 32.3 g
DS18B20 = 25.1 °C
SHT31-D found at 0x44
SHT31-D: air_temperature = 21.4
SHT31-D: air_humidity = 47.2
Wi-Fi link up, waiting for DHCP...
Got IP: 192.168.1.42/24
published Home Assistant discovery for node 'scale'
Published 32.3 to birds/scale/weight
Published 25.1 to birds/scale/temperature
Entering deep sleep for 10s
```

Idle cycles are much quieter — an empty feeder skips the radio, the DS18B20 and
the I²C sensors entirely, which is the whole point of the battery profile:

```
node 'scale' (Draußen) booted, battery profile
HX711 raw reading: 8388601
Entering deep sleep for 2s
```

**An air-quality node (`NODE=kueche`, mains)** stays associated and loops:

```
node 'kueche' (Küche) booted, mains profile
Wi-Fi link up, waiting for DHCP...
Got IP: 192.168.1.51/24
SDS011: pm25 = 8.3
SDS011: pm10 = 12.1
published Home Assistant discovery for node 'kueche'
Published 8.3 to smarthome/kueche/pm25
```

Weight is published in **grams** (converted on-device from the flash-stored
calibration); the `config:` line shows the values loaded from flash (or
built-in defaults on a blank device). Calibration and tuning are changed from
Home Assistant — see the [README](README.md#configure--calibrate-from-home-assistant).

### If a sensor is quiet

Every sensor is optional at runtime: one that does not answer is logged and
omitted, never fatal. What the log says, and what it usually means:

| Log line | Check |
| --- | --- |
| `HX711 not responding` | DT/SCK on D1/D0, and that the amp has power |
| `DS18B20 not responding; skipping temperature` | DATA on D2 **and the 4.7 kΩ pull-up to 3V3** |
| `no SHT31-D at 0x44 or 0x45 — check SDA/SCL and the pull-ups` | nothing ACKed on the bus: SDA=D4, SCL=D5, pull-ups present, sensor powered |
| `SHT31-D found at 0x45 (ADDR strapped high); using it` | nothing — the breakout straps ADDR high and the firmware adopted it |
| `no SCD41 at 0x62 …` | same bus checks; the SCD41 also needs a solid 3V3 supply, it draws ~200 mA in bursts |
| `SCD41 found …` but no readings on the first round | expected: the first periodic measurement needs ~5 s, it arrives next round |
| `SDS011 not responding; skipping its readings` | UART RX on D3 ← sensor TX, TX on D10 → sensor RX (not swapped), and the sensor on **5 V** |

The `found at 0x…` lines come from a one-shot I²C probe on the first measurement
of each boot, so a bus problem is visible before the first reading is even
attempted.

Subscribe on the broker side to confirm the payload:

```bash
# One node
mosquitto_sub -h <broker-ip> -t 'birds/scale/#' -v

# Everything the fleet publishes, including the discovery configs
mosquitto_sub -h <broker-ip> -t 'smarthome/#' -t 'birds/#' -t 'homeassistant/#' -v
```

The entities appear in Home Assistant by themselves — each node publishes
retained discovery configs on its first connect after a power-up, for its
readings *and* for its calibration/tuning controls. Nothing is declared by hand;
see [`home-assistant/README.md`](home-assistant/README.md) for the entity list
and for calibrating the scale.

> **Re-flashing does *not* re-announce discovery.** The "already announced" flag
> lives in RTC fast RAM, which survives both the flash and the reset that
> follows it — confirmed on hardware. Only removing power clears it, so to force
> a re-announce, **unplug the USB cable and plug it back in**. A board that has
> published its discovery configs once will otherwise stay quiet on that topic
> however many times you flash it.
>
> If Home Assistant has lost the entities and you also want the broker's copies
> gone, clear the retained configs first with
> `mosquitto_pub -h <broker-ip> -t 'homeassistant/sensor/<node>/<key>/config' -r -n`,
> then power-cycle.

---

## 7. Provisioning a board without reflashing

`NODE=` is only the identity a board *starts* with. Any board can be told to
become another node over MQTT — useful to repurpose one, to swap in a spare, or
to flash the whole fleet with one image and sort out which is which afterwards.

Every boot prints the topic to address that board on. It is keyed by MAC,
because that is the only name a board is sure of before it knows anything else:

```
node 'scale' (Draußen) booted, battery profile
provision topic: smarthome/provision/a1b2c3d4e5f6
```

```bash
# Become the kitchen node
mosquitto_pub -h <broker-ip> -r -t smarthome/provision/a1b2c3d4e5f6 -m kueche

# Go back to the identity it was flashed with
mosquitto_pub -h <broker-ip> -r -t smarthome/provision/a1b2c3d4e5f6 -m default
```

**Publish it retained** (`-r`). The node only looks while it is online for a
publish, so for a sleeping battery node the retained message is what gets the
change delivered on its next wake-up. Then:

```
provisioned as node 'kueche'; restarting
…
node 'kueche' (Küche) booted, mains profile
provisioned as node 'kueche' (from flash)
```

Notes:

- The restart is deliberate — the sensor set decides which buses are brought up,
  which happens during boot.
- A name the firmware doesn't know is logged and ignored, and the board carries
  on as it was: `provisioning asked for unknown node 'kuche'; expected one of: …`
- The retained message is re-delivered on every connect, so the firmware writes
  flash only when the value actually changes.
- The identity lives in its own flash sector, so provisioning a board never
  disturbs a scale's stored tare/calibration.
- Remember to clear the retained message (`-r -n`) if you later hand the board
  to a different room by reflashing, or it will provision itself back.
