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
> scale's HX711 wiring is the exception: that part has been running. Its battery
> divider and protection board are new and unbuilt, like everything else here.

## The board

Seeed Studio XIAO ESP32-C3. The silkscreen numbers the pads `D0`–`D10`, which
are *not* the GPIO numbers the datasheet and the firmware use:

| Pad | GPIO | Used for | On which node |
| --- | --- | --- | --- |
| D0  | 2  | HX711 SCK | `draussen` |
| D1  | 3  | HX711 DT | `draussen` |
| D2  | 4  | Battery divider tap (ADC1); a DS18B20 1-Wire line on a node that has one instead | `draussen` |
| D3  | 5  | UART RX ← SDS011 TX | `wohnzimmer` |
| D4  | 6  | I²C SDA | every node but none exclusively — `draussen`, `schlafzimmer`, `wohnzimmer`, `kueche`, `bad` |
| D5  | 7  | I²C SCL | as SDA |
| D6  | 21 | — **console UART TX** | keep free |
| D7  | 20 | — **console UART RX** | keep free |
| D8  | 8  | — | free |
| D9  | 9  | — **BOOT button** | keep free |
| D10 | 10 | UART TX → SDS011 RX | `wohnzimmer` |

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

## `NODE=bad` / `NODE=kueche` — SHT31-D only

The simplest build, and the right one to start with: four wires and one sensor.
If this works, the I²C half of every other node works.

Identical hardware; the two names differ only in which room they publish as.
Both are rooms that get wet on purpose, where the signal worth a long series is
the humidity and how long it takes to fall again after a shower or a pot.

> The kitchen carried the SDS011 particulate sensor until it moved to
> `wohnzimmer` — see that section for why, if you are looking at an older build
> or an older note.

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

**Expected at boot** (`kueche` says `Küche` in place of `Bad`):

```
node 'bad' (Bad) booted, mains profile
SHT31-D found at 0x44
SHT31-D: temperature = 21.4
SHT31-D: humidity = 48.2
```

If instead you get `no SHT31-D at 0x44 or 0x45`, the firmware immediately
sweeps the whole bus and tells you which of two very different problems you
have:

```
I²C scan: nothing answered between 0x08 and 0x77 — the bus itself is not working
```

means SDA and SCL swapped, no pull-ups, or the breakout not actually powered.
Whereas:

```
I²C scan: 0x76 answered
I²C scan: the bus works, so the missing sensor is at none of those addresses
```

means the wiring is fine and you have a different device fitted, or one that
straps its address somewhere unexpected.

---

## `NODE=schlafzimmer` — SCD41 **and** SHT31-D

The living room is this build plus a particulate sensor; everything in this
section applies there too, so read it first and then the `wohnzimmer` section
for what it adds.

> **Do not peel the white film off the SCD41's metal cap.** It is not a
> protective sticker or a shipping label — it is the gas-permeable membrane over
> the sensor opening, and it is part of the sensor. Removing it leaves the
> optical path exposed, and the sensor then answers every command perfectly
> while reporting `0 ppm` for ever. That failure cost a module on 2026-08-26 and
> looks exactly like a wiring or supply fault from the outside, so it is worth
> being sure: the film stays on.

**You need:** XIAO ESP32-C3, SCD41 breakout, SHT31-D breakout, USB-C supply.

### Why two sensors

The SCD41 has a temperature and humidity sensor built in, and it is the weaker
one. Sensirion specifies it at **±6 %RH** (±9 outside 15–35 °C / 20–65 %RH)
against the SHT31-D's **±2 %**, because it sits on a die that heats itself for
the CO₂ measurement. So the SHT31-D measures the room, and the SCD41 measures
CO₂ — plus its own temperature and humidity, which are published separately
under `scd41_` because you need them to calibrate its offset (below).

### Wiring

Both sensors share one I²C bus. Four wires leave the XIAO; both breakouts tap
them.

| Sensor pin | XIAO pad | GPIO | Note |
| --- | --- | --- | --- |
| VIN / VDD | 3V3 | — | see *Supply* |
| GND | GND | — | |
| SDA | D4 | 6 | both sensors in parallel |
| SCL | D5 | 7 | both sensors in parallel |

**Addresses** do not collide: the SCD41 is fixed at `0x62`, the SHT31-D sits at
`0x44` or `0x45` and the firmware adopts whichever answers. Nothing to strap.

**Pull-ups.** Both breakouts usually fit their own, which puts them in parallel:
two 10 kΩ become 5 kΩ, and even 2.2 kΩ against 10 kΩ lands near 1.8 kΩ. That is
still inside the 3 mA sink current the SCD4x datasheet guarantees its low level
for, so leave both fitted and only unsolder one set if the bus actually
misbehaves.

**Supply — and this is the one place the topology matters.** The SCD41 draws
175 mA typical, 205 mA maximum while its IR source fires, against microamps
between measurements. Sensirion allows **30 mV** of ripple at the sensor, which
over a 205 mA pulse is a total resistance budget of about **146 mΩ** — for the
regulator, both leads, and every contact on the way. Four DuPont contacts alone
eat most of that, which is why this node wants soldered joints rather than
jumper headers. Ten centimetres of 26 AWG is only ~27 mΩ for the pair, so length
is cheap and distance from the board is affordable.

Run 3V3 and GND from the XIAO **directly to the SCD41** as their own pair rather
than daisy-chaining its supply through the SHT31-D breakout — that would put
extra pads and contacts in the path that carries the pulse. The SHT31-D draws
almost nothing and may branch off anywhere. SDA and SCL carry milliamps, so
those can be looped through in any order.

```
XIAO   3V3 ─────────────────► SCD41 VDD      own pair, short, soldered
       GND ─────────────────► SCD41 GND
        └┬─────────────────► SHT31 VIN       may branch off
         └─────────────────► SHT31 GND

       D4  ──┬─────────────► SCD41 SDA       bus, order irrelevant
             └─────────────► SHT31 SDA
       D5  ──┬─────────────► SCD41 SCL
             └─────────────► SHT31 SCL
```

### Placement

**Both sensors in the same air, both away from the board.** Same air, because
the offset calibration below is the difference between their two temperatures —
mount them next to each other or it measures nothing useful. Away from the
board, because Sensirion's design-in guide names the Wi-Fi module explicitly as
a heat source and asks for maximum distance from self-heating components; a
fixed offset cannot compensate a heat source that varies with radio traffic.

Otherwise as for any air sensor: not inside a sealed enclosure, not where
someone breathes directly on it. The SCD41's automatic self-calibration assumes
the room reaches roughly outdoor CO₂ at some point in a week — a room that is
never aired will drift.

**Power.** Mains, always on. The firmware runs the SCD41 in *periodic* mode on
mains, which is what its self-calibration expects, and samples every 60 s.

### Expected at boot

```
node 'schlafzimmer' (Schlafzimmer) booted, mains profile
SHT31-D found at 0x44
SCD41 found at 0x62
SCD41 serial 0x41AC3D073BD4
```

The serial number is worth a glance. It identifies the physical sensor, and it
is the cheapest counterfeit check available — the SCD4x is widely copied, and
fakes tend to answer with zeroes or with the same number on every unit, which
the firmware calls out. Two genuine modules must return different numbers.

The **first SCD41 round after a power-up reports nothing** — the first
conversion needs about five seconds and the firmware does not block the publish
path waiting for it. Readings appear on the next round:

```
SHT31-D: temperature = 26.3
SHT31-D: humidity = 52.9
SCD41: co2 = 367
SCD41: scd41_temperature = 26.6
SCD41: scd41_humidity = 55.9
```

### Calibrating the temperature offset

The SCD41 cancels its own self-heating with a temperature offset, 4 °C out of
the box — a figure chosen for continuous operation in a particular enclosure,
not for yours. It is not cosmetic: the humidity output is compensated to the
offset-corrected temperature, so an offset that does not match the real
self-heating skews **both** signals, temperature low and humidity high. The
datasheet only claims its RH/T accuracy if the offset is set correctly.

Do this once per node, after the sensors are mounted where they will live:

1. Let it run **at least 15 minutes** in place. Sensirion asks for that much for
   complete thermal equilibration, and the reading really does still drift for
   the first several minutes.
2. Read `temperature` (the SHT31-D) and `scd41_temperature` off the device card.
3. New offset = `4.00 + (scd41_temperature − temperature)`, i.e. subtract however
   much the SCD41 reads *colder* than the reference.
4. Enter it in the **Temperatur-Offset** number entity on the node's device card.

The value is written to the sensor on the next round — which costs one round,
because applying it stops and restarts the measurement. It is deliberately not
persisted to the sensor's own EEPROM (limited write cycles); the firmware keeps
it in flash and reprograms it on every boot.

Calibrate in the final position and enclosure. On a bench, dangling off a USB
cable, the self-heating is different and so is the answer.

### If something is wrong

`no SCD41 at 0x62` or `no SHT31-D at 0x44 or 0x45` — read the bus scan that
follows it, same two cases as for the SHT31-D-only node above.

If a sensor is *found* but produces no readings, the log says where it fell
over rather than just "not responding": whether it refused to start, never
reported a ready measurement, answered with a bad CRC, or returned a well-formed
measurement it considers invalid (`reported 0 ppm`). After three empty rounds
the firmware runs the SCD41's built-in self test, which settles the only
question that matters:

```
SCD41 self test reports a malfunction — the part itself is faulty
SCD41 self test passed — the sensor believes it is healthy, so look at
wiring, supply or placement rather than the part
```

---

## `NODE=wohnzimmer` — SCD41, SHT31-D **and** SDS011

The busiest mains node, and the only one on two buses at once. Build the
`schlafzimmer` node above first — the I²C half is identical, down to the
membrane warning and the supply budget — and then add the SDS011.

**You need:** everything the bedroom node needs, plus an SDS011 and a USB-C
supply that can spare ~100 mA more for its fan.

### Why the particulate sensor is here and not in the kitchen

The kitchen is where the particulates come from, which makes it the obvious
place and the wrong one.

The SDS011 is a nephelometer: an open optical path with a fan pulling room air
through it. Kitchen air carries fat aerosol from frying, and that films the
optics and the fan blade. The sensor does not fail when this happens — it keeps
answering, with a calibration that has quietly walked off. That is the same
shape of failure as the SCD41's membrane, and it is the expensive one, because
nothing in the log says it is happening.

The second reason is that the interesting question is answerable here and not
there. That frying makes smoke is not worth a year of data. How much of it
reaches the room people sit in, and how long it takes to clear, is — and it is
the number an automation can act on.

It also lands where an SHT31 already is, which is what makes the humidity
correction below possible at all.

### SDS011 → XIAO

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

**Nothing shared with the I²C sensors** but the supply rails. Keep the SDS011's
own 5V/GND pair away from the run that feeds the SCD41, which has a 146 mΩ
budget of its own (see the bedroom section) and does not want a fan's current
steps in it.

### Fan life, and the two cadences

The fan and laser are rated around 8000 hours, so the firmware keeps them asleep
and wakes them only to measure. On this node that matters more than it used to,
because the SCD41 beside it wants a reading a minute and the SDS011 must not
have one.

So the node has a **base round of 60 s** — the SCD41's and the SHT31's cadence —
and the SDS011 sits out fourteen of every fifteen of them, giving it one round
every 15 minutes. That is `Slot::every(900)` in
[`src/node.rs`](../src/node.rs); Home Assistant is told about both cadences
separately, so the PM entities are not marked stale during the fourteen minutes
they are legitimately quiet.

Do not "helpfully" leave the fan running.

### Humidity correction

A nephelometer infers mass from scattered light. Above roughly 60 %RH
hygroscopic particles take up water and scatter like something bigger than
their dry selves, so the reported mass runs high — steeply so as the air
approaches saturation. Without a humidity figure you cannot tell a dusty room
from a damp one.

Because this node has an SHT31, the firmware corrects for it, using the
κ-Köhler growth factor:

```text
C_dry = C_wet / (1 + κ · (RH/100) / (1 − RH/100))
```

Both values are published: `pm25` / `pm10` are corrected — build automations on
those — and `pm25_raw` / `pm10_raw` are what the sensor actually said. You need
the pair to judge the correction, exactly as you need `scd41_temperature` to
judge the CO₂ sensor's offset.

**κ is a property of your air, not of the sensor.** Published values run from
about 0.25 to 0.62 for ambient aerosol; indoor aerosol sits at the low end, so
the default is **0.25** — a starting point, not a calibration. It is a slider in
Home Assistant (*Feuchte-Korrektur κ*, `smarthome/wohnzimmer/config/sds011_kappa`):
watch corrected against raw over a few humid days and move it. Setting it to `0`
turns the correction off and leaves both entities in place.

Two limits worth knowing. Above **95 %RH** the correction is capped — the growth
factor has a pole at 100 %, and air that wet is fog, where the model's premise
has stopped holding anyway. And if the SHT31 does not answer in a round, only
the raw values are published; the corrected ones expire rather than being
quietly filled with uncorrected numbers.

### Placement

The I²C pair goes as in the bedroom section. The **SDS011 belongs next to the
SHT31**, close enough that the humidity it is corrected against is the air it
actually sampled — that is what makes the correction mean anything.

Beyond that: airflow in and out unobstructed, and the sensor must stay dry.
Condensation ruins both the reading and the hardware.

### Expected at boot

```
node 'wohnzimmer' (Wohnzimmer) booted, mains profile
SHT31-D found at 0x44
SCD41 found at 0x62
SCD41 serial 0x41AC3D073BD4
```

Nothing about the SDS011 — it is only touched on a round of its own. What you
should *not* see is:

```
SDS011 UART init failed: ...; sensor disabled
```

which means the UART could not be configured at all — a build or pin problem,
not a wiring one. A sensor that is wired wrong instead shows up, once every 15
minutes, as:

```
SDS011 not responding; skipping its readings
```

A working PM round takes 10–30 s from the fan starting, then:

```
SDS011: pm25_raw = 24.5
SDS011: pm10_raw = 100.0
SDS011: pm25 = 18.1
SDS011: pm10 = 74.1
```

The corrected values being *below* the raw ones is the expected direction: the
correction only ever removes water, never adds particles.

---

## `NODE=draussen` — the bird scale

The busiest board: two sensors, two buses, a battery and the only analogue
measurement in the fleet. This is the one that already exists; the others are
simplifications of it.

**You need:** XIAO ESP32-C3, 1 kg straight-bar load cell, HX711 breakout,
SHT31-D breakout, 1S protection board, two 100 kΩ resistors, one 100 nF
capacitor, LiPo cell.

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

### SHT31-D → XIAO

Exactly as for `NODE=bad`: 3V3, GND, SDA→D4, SCL→D5.

On this node the SHT31-D's readings are published under `air_temperature` and
`air_humidity`. That prefix is older than the current build — it was there to
leave the plain `temperature` key to a DS18B20 that is no longer fitted — and it
stays because renaming the entity would orphan its history in Home Assistant.
Nothing about the wiring changes; it is worth knowing when you go looking for
the entity.

### No DS18B20 on this node

The probe used to sit on D2 with a 4.7 kΩ pull-up. The battery divider below
needs the same pad, and there is no second candidate (see *Battery sense*), so
the probe came off. If you are rebuilding an older board: **remove the 4.7 kΩ**,
or it will pull the divider's tap towards 3V3 and every voltage reading with it.

The DS18B20 driver and its node slot are still in the firmware — the pin is a
per-node choice, and the build fails if any node ever enables both.

### Power

A LiPo on `B+`/`B-` through the XIAO's onboard charger. This is the only battery
node: it cold-boots out of deep sleep every couple of seconds, reads the load
cell, and only brings up Wi-Fi when there is weight on the pan or the heartbeat
is due.

Two consequences for how you build it:

- **The 5V pad is dead** on battery, which is why the SDS011 is not on this node.
- **Keep the sensors on 3V3 from the XIAO**, so they go down with it in sleep.

#### Protection board

The XIAO charges the cell but does **not** protect it: there is no low-voltage
cutoff, so a flat battery keeps being drained until it is damaged. Unless your
pack already has a protection PCB tucked under the tape at the tab end — many
do — put a 1S board inline between the cell and the board.

```
LiPo  +  ──►  B+  ┌──────────────────┐  P+  ──►  XIAO  B+
                  │  1S protection    │
LiPo  −  ──►  B−  └──────────────────┘  P−  ──►  XIAO  B−
                      (B = battery)        (P = load)
```

**Read the silkscreen before soldering.** Plenty of 1S boards expose only three
pads — `B+`, `B-`, `P-` — because the positive rail passes straight through and
the MOSFETs switch the *negative* side. On those, the XIAO's `B+` comes from the
board's `B+` and only the negative goes through `P-`. Both layouts are fine;
wiring one as though it were the other is not.

Solder the board to the cell first (`B-`, then `B+`), insulate it, and only then
run the load wires to the XIAO — a bare 2000 mAh pouch will deliver tens of amps
into a slipped iron. Nothing on the XIAO side has reverse-polarity protection.

What such a board does **not** do is keep the cell healthy. The common DW01A-class
part cuts off around 2.4-2.5 V, far below the ~3.0 V where a LiPo starts losing
capacity for good. It is a safety device against fire and deep-discharge damage,
not a longevity one. Noticing the difference is what the divider below is for.

#### Battery sense

**The XIAO has no battery-sense path at all** — `B+` reaches the charger and the
regulator, never an ADC — so reading the cell means fitting a divider:

```
  XIAO B+ (= P+)
        │
      [ R1 ]  100 kΩ
        │
        ├──────────────────►  D2 / GPIO4
        │
        ├────────┐
        │        │
      [ R2 ]   [ C ]
      100 kΩ   100 nF
        │        │
  XIAO GND ──────┴───────  (= P−)
```

As a netlist, which is harder to misread than any diagram:

| Part | From | To |
| --- | --- | --- |
| R1 100 kΩ | XIAO `B+` | node **A** |
| R2 100 kΩ | node **A** | XIAO `GND` |
| C 100 nF | node **A** | XIAO `GND` |
| wire | node **A** | XIAO `D2` |

Node **A** is the tap: four legs meet there, and the two resistors are always in
series between `B+` and ground — never a bridge across one of them.

**Why D2, and why it costs the probe its pin.** The divider needs an ADC1 input.
On the ESP32-C3 those are GPIO2/3/4 = `D0`, `D1`, `D2`, and the HX711 has the
first two. `D3` is on ADC2, which is unusable while Wi-Fi is up, so `D2` is the
only candidate — and the DS18B20 wanted it. The SHT31-D measures air temperature
anyway. `node.rs` fails the build if a node ever asks for both.

**Why the foot goes to `GND` and not to the cell.** The XIAO's ground is the
protection board's `P−`, the switched side. Tie the divider there and the
low-voltage cutoff switches it off along with everything else; tie it straight to
the cell's `B−` and it keeps drawing ~21 µA *after* the cutoff — exactly the deep
discharge the board was fitted to prevent.

**Numbers.** 4.2 V full becomes 2.1 V at the pin, 3.0 V empty becomes 1.5 V; both
sit inside the ~2.5 V that 11 dB attenuation spans, with no over-range while
charging. The 100 nF supplies the ADC's sampling charge, so the 50 kΩ source
impedance does not skew the reading. Standing draw is ~21 µA, about 184 mAh a
year — roughly 9 % of a 2000 mAh pack. Two 1 MΩ resistors would cut that to ~2 µA
if it ever matters.

Use 1 % metal film. The firmware calibrates the *ADC* against the chip's eFuse
reference, so readings arrive in millivolts already corrected for that part — but
nothing corrects the resistors. If a multimeter disagrees, the fix is
`R_TOP_KOHM` / `R_BOTTOM_KOHM` in [`src/battery.rs`](../src/battery.rs) and a
reflash; there is no runtime knob for it yet.

The reading is published as `birds/scale/battery_voltage`, and the log warns
below 3.0 V. A reading under 2.0 V is not published at all — that is not a flat
cell, it is a divider that is not there, and it says so:

```
battery reads 41 mV, which is no cell at all — check the divider is fitted
between B+ and GND with its tap on D2, and that a cell is connected
```

### Outdoors

Nothing on this node is waterproof, so all of it needs an enclosure — but the
SHT31-D needs to *breathe*, or it reports the humidity of the inside of your box.
A vent with a membrane, or a shielded underside opening, not a sealed lid.

**Expected at boot:**

```
node 'scale' (Draußen) booted, battery profile
provision topic: smarthome/provision/a1b2c3d4e5f6
SHT31-D found at 0x44
HX711 raw reading: 8402913
battery = 4.03 V
```

Note the node **id** is `scale`, not `draussen` — the name selects it at build
time, the id is what appears in topics, kept from before the fleet existed.

---

## Which pads stay free

Useful if you want to add something, or are checking you have not double-booked
a pin:

| Node | Free pads |
| --- | --- |
| `bad`, `kueche` | D0, D1, D2, D3, D8, D10 |
| `schlafzimmer` | D0, D1, D2, D3, D8, D10 |
| `wohnzimmer` | D0, D1, D2, D8 |
| `draussen` | D3, D8, D10 |

`D6`, `D7` and `D9` are excluded everywhere: console UART and the BOOT button.

## Before you power it up

1. Continuity from every module's GND to the XIAO's GND.
2. No 5 V anywhere near a GPIO — only the SDS011's supply pin.
3. SDA and SCL not swapped (the single commonest mistake on these boards).
4. SDS011 TX→RX crossed.
5. On `draussen`: the divider's foot on the XIAO's `GND` (the protection board's
   `P−`), not on the cell's `B−` — and no 4.7 kΩ left over from a DS18B20 on D2.

Then plug in USB and watch the log — every node reports what it found on its
buses within the first second of booting, which is precisely so that a wiring
mistake is visible before the first reading is even attempted. See
[FLASHING.md](../FLASHING.md) for getting the firmware onto the board, and
[the platform notes](base-platform.md) for why the fleet is shaped this way.
