# Wiring, per node

One firmware image, five different boards. What you solder depends on which
`NODE=` the board is going to be — this page has the complete wiring for each,
so you should not have to read the source to build one.

The pin assignments here are taken from the firmware
([`src/platform.rs`](../src/platform.rs) and [`src/main.rs`](../src/main.rs)); if
the two ever disagree, the source wins and this page is the bug.

> **None of this has been built and tested yet.** The pin map, the pull-ups and
> the supply choices are what the firmware expects and what the datasheets say —
> not what a working board on a bench has confirmed. Check each connection
> against your own modules' silkscreen before powering anything up. The bird
> scale's HX711 and DS18B20 wiring is the exception: that combination has been
> running.

## The board

Seeed Studio XIAO ESP32-C3. The silkscreen numbers the pads `D0`–`D10`, which
are *not* the GPIO numbers the datasheet and the firmware use:

| Pad | GPIO | Used for | On which node |
| --- | --- | --- | --- |
| D0  | 2  | HX711 SCK | `draussen` |
| D1  | 3  | HX711 DT | `draussen` |
| D2  | 4  | DS18B20 data (1-Wire) | `draussen` |
| D3  | 5  | UART RX ← SDS011 TX | `kueche` |
| D4  | 6  | I²C SDA | `draussen`, `schlafzimmer`, `wohnzimmer`, `bad` |
| D5  | 7  | I²C SCL | `draussen`, `schlafzimmer`, `wohnzimmer`, `bad` |
| D6  | 21 | — **console UART TX** | keep free |
| D7  | 20 | — **console UART RX** | keep free |
| D8  | 8  | — | free |
| D9  | 9  | — **BOOT button** | keep free |
| D10 | 10 | UART TX → SDS011 RX | `kueche` |

`D6`/`D7` carry the log output. `D9` is the BOOT button, which the bootloader
samples at reset — pulling it low with your own wiring puts the board into
download mode instead of running the firmware. `GPIO0`/`GPIO1` are not broken
out on this board at all.

Power pads: `3V3` (regulated output, and also the input if you feed it from a
regulated supply), `5V` (tied to USB VBUS — an *output* when USB is plugged in),
`GND`, and `B+`/`B-` on the underside for a LiPo through the onboard charger.

## Rules that apply everywhere

- **One ground.** Every module's `GND` goes to the XIAO's `GND`. Sensors on
  their own supply still need their ground tied to the board's, or nothing
  works and the failure looks random.
- **3V3 unless stated.** Only the SDS011 wants 5 V.
- **Nothing above 3.3 V on a GPIO.** The ESP32-C3 is not 5 V tolerant.
- **Keep sensor leads short**, especially I²C. The bus runs at 100 kHz
  (`I2cConfig::default()`), which is forgiving, but 20 cm of unshielded ribbon
  next to a switching supply is not a bus.
- **Wire it with the board unpowered**, then plug USB in last.

---

## `NODE=bad` — SHT31-D only

The simplest build, and the right one to start with: four wires and one sensor.
If this works, the I²C half of every other node works.

**You need:** XIAO ESP32-C3, SHT31-D breakout, USB-C supply.

| SHT31-D pin | XIAO pad | GPIO | Note |
| --- | --- | --- | --- |
| VIN / VCC | 3V3 | — | |
| GND | GND | — | |
| SDA | D4 | 6 | |
| SCL | D5 | 7 | |
| ADDR | — | — | leave unconnected, see below |

**Address.** The firmware probes `0x44` (ADDR low) and `0x45` (ADDR high) and
adopts whichever answers, so you do not have to care which way your breakout
straps it — but you do have to leave the strap alone rather than floating it
against something. Most breakouts pull ADDR low on board.

**Pull-ups.** SDA and SCL need pull-ups to 3V3. Nearly every SHT31-D breakout
has them fitted (usually 10 kΩ); if yours does not, add 4.7 kΩ on each line.

**Power.** Mains — a USB-C phone charger. This node never sleeps and samples
every 120 s.

**Expected at boot:**

```
node 'bad' (Bad) booted, mains profile
SHT31-D found at 0x44
SHT31-D: temperature = 21.4
SHT31-D: humidity = 48.2
```

If instead you get `no SHT31-D at 0x44 or 0x45 — check SDA/SCL and the
pull-ups`, the usual causes in order: SDA and SCL swapped, no pull-ups, or the
breakout not actually powered.

---

## `NODE=schlafzimmer` / `NODE=wohnzimmer` — SCD41 only

Identical hardware; the two names differ only in which room they publish as.

**You need:** XIAO ESP32-C3, SCD41 breakout, USB-C supply.

| SCD41 pin | XIAO pad | GPIO |
| --- | --- | --- |
| VIN / VDD | 3V3 | — |
| GND | GND | — |
| SDA | D4 | 6 |
| SCL | D5 | 7 |

**Address** is fixed at `0x62`; there is nothing to strap.

**Supply.** The SCD41 pulls a substantial burst of current while it measures —
far more than its idle draw. Feed it from the XIAO's `3V3` pad with short wires,
and do not put it at the end of a long thin lead shared with anything else. A
brown-out here shows up as a sensor that answers the probe and then returns
nothing.

**Placement matters more than the wiring.** It measures the air it sits in: not
inside a sealed enclosure, not in the exhaust of the board's own warmth, and not
where someone breathes directly on it. Its automatic self-calibration assumes
the room reaches roughly outdoor CO₂ at some point in a week — a room that is
never aired will drift.

**Power.** Mains, always on. The firmware runs the sensor in *periodic* mode on
mains, which is what its self-calibration expects, and samples every 60 s.

**Expected at boot:**

```
node 'schlafzimmer' (Schlafzimmer) booted, mains profile
SCD41 found at 0x62
```

The **first round after a power-up reports nothing** — the first conversion
needs about five seconds and the firmware does not block the publish path
waiting for it. Readings appear on the next round:

```
SCD41: co2 = 812
SCD41: temperature = 21.4
SCD41: humidity = 48.2
```

If you get `no SCD41 at 0x62`, check the same three things as for the SHT31-D.

---

## `NODE=kueche` — SDS011 particulate sensor

The only node with a UART, the only one needing 5 V, and the only one with a
moving part.

**You need:** XIAO ESP32-C3, SDS011, USB-C supply that can spare ~100 mA extra
for the fan.

| SDS011 pin | XIAO pad | GPIO | Direction |
| --- | --- | --- | --- |
| 5V | 5V | — | supply |
| GND | GND | — | |
| TXD | D3 | 5 | sensor → board |
| RXD | D10 | 10 | board → sensor |
| 1µm / 2.5µm | — | — | leave unconnected |

**Crossed, not straight.** The sensor's TX goes to the board's RX and vice
versa. Getting this backwards is silent: no error, just no frames.

**The 5V pad is USB VBUS**, so this node only works while it is plugged into
USB — which it is, being a mains node. If you power the XIAO from a regulated
3V3 supply instead, the `5V` pad gives you nothing and the SDS011 needs its own
5 V source (with the grounds tied together).

**Logic levels.** The SDS011's UART is 3.3 V logic despite the 5 V supply, so no
level shifter is needed in either direction. This is what the module's datasheet
states; if you have a meter, confirming TXD idles at ~3.3 V and not ~5 V before
connecting it to D3 costs a minute and could save the pin.

**Fan life is the scarce resource.** It is rated around 8000 hours, so the
firmware keeps it asleep and wakes it only to measure — 15 minutes between
rounds, and it ends the warm-up as soon as consecutive readings agree rather
than after a fixed wait. Do not "helpfully" leave it running.

**Placement.** Airflow in and out must be unobstructed, and the sensor must stay
dry: condensation ruins both the reading and the hardware. Not directly over a
kettle or a hob.

**Expected at boot** — nothing about the SDS011, because it is only touched on a
publish round. What you should *not* see is:

```
SDS011 UART init failed: ...; sensor disabled
```

which means the UART could not be configured at all — a build or pin problem,
not a wiring one. A sensor that is wired wrong instead shows up per round as:

```
SDS011 not responding; skipping its readings
```

A working round takes 10–30 s from the fan starting, then:

```
SDS011: pm25 = 24.5
SDS011: pm10 = 100.0
```

---

## `NODE=draussen` — the bird scale

The busiest board: three sensors, two buses and a battery. This is the one that
already exists; the others are simplifications of it.

**You need:** XIAO ESP32-C3, 1 kg straight-bar load cell, HX711 breakout,
DS18B20 waterproof probe, SHT31-D breakout, one 4.7 kΩ resistor, LiPo cell.

### Load cell → HX711

The four leads go to the HX711's **input** side. Colours follow the common
straight-bar convention — check against your cell's own datasheet, they vary.

| Load-cell lead | HX711 pin | Meaning |
| --- | --- | --- |
| Red | E+ | excitation + |
| Black | E− | excitation − |
| Green | A+ | signal + |
| White | A− | signal − |

If loading the pan makes the reading go *down*, swap A+ and A−.

### HX711 → XIAO

| HX711 pin | XIAO pad | GPIO | Direction |
| --- | --- | --- | --- |
| VCC | 3V3 | — | |
| GND | GND | — | |
| SCK | D0 | 2 | board → amp |
| DT | D1 | 3 | amp → board |

`DT` is configured with the internal pull-up on, so a *disconnected* amplifier
reads as permanently "not ready" and times out cleanly instead of feeding you
floating garbage. That is deliberate: a scale reading nonsense is worse than one
reading nothing.

### DS18B20 → XIAO

| DS18B20 lead | XIAO pad | GPIO | Note |
| --- | --- | --- | --- |
| Red (VCC) | 3V3 | — | |
| Black (GND) | GND | — | |
| Yellow (DATA) | D2 | 4 | **plus 4.7 kΩ from DATA to 3V3** |

The pull-up is **not optional**. The internal one is enabled as a backup, but it
is far too weak for the metre or so of probe cable; without the external
resistor you get intermittent CRC failures that look like a flaky sensor.

### SHT31-D → XIAO

Exactly as for `NODE=bad`: 3V3, GND, SDA→D4, SCL→D5.

On this node the SHT31-D's readings are published under `air_temperature` and
`air_humidity`, because the DS18B20 already owns the plain `temperature` key —
the probe measures the feeder, the SHT31-D measures the air. Nothing about the
wiring changes; it is worth knowing when you go looking for the entity in Home
Assistant.

### Power

A LiPo on `B+`/`B-` through the XIAO's onboard charger. This is the only battery
node: it cold-boots out of deep sleep every couple of seconds, reads the load
cell, and only brings up Wi-Fi when there is weight on the pan or the heartbeat
is due.

Two consequences for how you build it:

- **The 5V pad is dead** on battery, which is why the SDS011 is not on this node.
- **Keep the sensors on 3V3 from the XIAO**, so they go down with it in sleep.

### Outdoors

The DS18B20 is the only waterproof part. The board, the HX711 and the SHT31-D
all need an enclosure — but the SHT31-D needs to *breathe*, or it reports the
humidity of the inside of your box. A vent with a membrane, or a shielded
underside opening, not a sealed lid.

**Expected at boot:**

```
node 'scale' (Draußen) booted, battery profile
provision topic: smarthome/provision/a1b2c3d4e5f6
SHT31-D found at 0x44
HX711 raw reading: 8402913
```

Note the node **id** is `scale`, not `draussen` — the name selects it at build
time, the id is what appears in topics, kept from before the fleet existed.

---

## Which pads stay free

Useful if you want to add something, or are checking you have not double-booked
a pin:

| Node | Free pads |
| --- | --- |
| `bad` | D0, D1, D2, D3, D8, D10 |
| `schlafzimmer`, `wohnzimmer` | D0, D1, D2, D3, D8, D10 |
| `kueche` | D0, D1, D2, D4, D5, D8 |
| `draussen` | D3, D8, D10 |

`D6`, `D7` and `D9` are excluded everywhere: console UART and the BOOT button.

## Before you power it up

1. Continuity from every module's GND to the XIAO's GND.
2. No 5 V anywhere near a GPIO — only the SDS011's supply pin.
3. SDA and SCL not swapped (the single commonest mistake on these boards).
4. SDS011 TX→RX crossed.
5. The DS18B20's 4.7 kΩ actually fitted, and to 3V3 rather than to 5 V.

Then plug in USB and watch the log — every node reports what it found on its
buses within the first second of booting, which is precisely so that a wiring
mistake is visible before the first reading is even attempted. See
[FLASHING.md](../FLASHING.md) for getting the firmware onto the board, and
[the platform notes](base-platform.md) for why the fleet is shaped this way.
