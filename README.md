# rs-smarthome-nodes

Async `no_std` Rust ([Embassy](https://embassy.dev)) firmware for a fleet of
**Seeed Studio XIAO ESP32-C3** smart-home sensor nodes. One image serves every
node: `NODE=<name>` at build time selects which sensors are populated, what the
node is called, and whether it sleeps between readings or stays associated.
Home Assistant picks the nodes up automatically over **MQTT
auto-discovery** — no hand-declared entities.

It started as a battery bird-feeder scale, which is still the default node
(`NODE=terrasse`): on each wake-up it reads a load cell via an HX711 amplifier
and compares it against a tare baseline kept in RTC RAM. While the feeder is
empty it polls every couple of seconds from **light** sleep — no radio, and no
cold boot per poll — which bounds how short a visit it can see at all. The
amplifier is powered down between those polls, and its settling time after
power-up is waited out asleep rather than awake. Once weight crosses the threshold it
stops sleeping and **watches the visit through awake**, so the published weight
is a settled median rather than whichever conversion happened to land first, and
the visit gets a real duration instead of one rounded to the sleep interval;
then it brings up Wi-Fi and publishes once (grams converted on-device). A
periodic **heartbeat** (default every 10 min) publishes anyway, so
Home Assistant always has a fresh reading. While online it also pulls any
retained calibration/tuning back from Home Assistant and persists it to flash.

```
┌──────────┐  bit-bang   ┌────────┐    Wi-Fi/MQTT (grams, °C)   ┌────────────────┐
│  Load    │────────────▶│ HX711  │──▶ ESP32-C3 ───────────────▶│ Home Assistant │
│  Cell    │  DT / SCK   │ 24-bit │       smarthome/terrasse/weight    │                │
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
| `terrasse` (default) | Terrasse | HX711 load cell + SHT31-D + cell voltage | battery, deep sleep |
| `schlafzimmer` | Schlafzimmer | SCD41 + SHT31-D | mains |
| `wohnzimmer` | Wohnzimmer | SCD41 + SHT31-D + SDS011 + SGP41 | mains (fan) |
| `kueche` | Küche | SHT31-D | mains, duty-cycled |
| `bad` | Bad | SHT31-D | mains, duty-cycled |
| `terrasse` | Terrasse | none yet — the board is on the network while it is wired up | battery, deep sleep |

### Provisioning a board

The `NODE=` value is only the identity a board *starts* with. To repurpose one —
or to flash the whole fleet with a single image and sort out which is which
afterwards — publish its node name **retained** to the board's provisioning
topic, which is keyed by MAC (the one name a board knows before it knows
anything else) and printed on every boot:

```
node 'terrasse' (Terrasse) booted, battery profile
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

**Power profiles** decide the loop. *Battery* nodes cold-boot out of deep sleep,
measure, publish only when there is something to say, and sleep again, at the
runtime intervals below. *Mains* nodes stay associated and sample on a fixed
per-node cadence — CO₂ continuity and the SDS011's duty-cycled fan both rule out
deep sleep. A sensor whose own cadence is slower than the node's round says so
per slot (`Slot::every`), which is how `wohnzimmer` reads CO₂ every minute while
its fan runs four times an hour.

*Mains, duty-cycled* is the third: on a cable, but deep-sleeping between rounds
anyway. The reason is heat rather than power. A node that keeps Wi-Fi up draws
something like 80–110 mA without pause, which is roughly a third of a watt
warming the inside of a small box, and a node whose entire job is to report the
room's temperature cannot afford to warm the air it is measuring. `kueche` and
`bad` carry nothing that needs continuity, so they stop running between
readings, sleeping their own `sample_secs` — the cadence they already published
at, so no history gets a step in it. Any node that sleeps at all keeps the
`config/deep_sleep` switch, which holds it awake for bench testing.

## Hardware

| Component        | Detail                                             |
| ---------------- | -------------------------------------------------- |
| MCU              | Seeed XIAO ESP32-C3 (RISC-V, external Wi-Fi ant.)  |
| Battery          | 2000 mAh LiPo via the XIAO's onboard charger       |
| Sensor           | 1 kg straight-bar load cell (tension S-config)     |
| Amplifier        | HX711 24-bit ADC                                   |
| Temperature      | DS18B20 waterproof 1-Wire probe (stainless steel) |
| T / RH           | SHT31-D breakout, I²C `0x44`                       |
| Cell voltage     | 100 kΩ/100 kΩ divider + 100 nF on ADC1              |
| CO₂ / T / RH     | SCD41 breakout, I²C `0x62`                         |
| PM2.5 / PM10     | SDS011, UART 9600 8N1, **5 V supply** (fan)        |

### Pin map (XIAO ESP32-C3 silkscreen → GPIO)

| Pad | GPIO | Use |
| --- | --- | --- |
| D0  | 2  | HX711 SCK |
| D1  | 3  | HX711 DT |
| D2  | 4  | DS18B20 1-Wire (4.7 kΩ pull-up to 3V3) **or** battery divider tap (ADC1) |
| D3  | 5  | SDS011 UART RX ← sensor TX |
| D4  | 6  | I²C SDA (SHT31-D + SCD41) |
| D5  | 7  | I²C SCL |
| D10 | 10 | SDS011 UART TX → sensor RX |

The two I²C sensors share one bus (their addresses do not clash), and on
`wohnzimmer` that bus runs alongside the SDS011's UART — the only node using
both. The SDS011 deliberately avoids D6/D7 (GPIO21/20), which are the console
UART pads the log output uses.

D2 has two possible jobs and can only do one of them: the DS18B20's 1-Wire line,
or the battery divider's tap. ADC2 is unusable while Wi-Fi is up, and of the
ADC1 pins only D0/D1/D2 are broken out on this board — the first two are the
HX711's — so cell voltage costs the probe its pin. `NODE=terrasse` takes that
trade; the build fails if a node table entry ever asks for both.

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
`smarthome/terrasse/temperature` in °C.

**No node ships with this today.** The outdoor node traded its probe for the
battery sense below; the DS18B20 slot and driver stay, because the pin is a
per-node choice rather than a firmware one.

### Wiring (battery divider → XIAO ESP32-C3)

The XIAO charges a cell but cannot measure one: `B+` reaches the charger and the
regulator, never an ADC. An external divider from the battery rail to ground
gives ADC1 something to read.

| Component | From | To |
| --- | --- | --- |
| R1 100 kΩ | XIAO `B+` | tap |
| R2 100 kΩ | tap | XIAO `GND` |
| C 100 nF | tap | XIAO `GND` |
| wire | tap | GPIO4 / D2 |

Two details are not optional. The divider's foot goes to the **XIAO's ground**,
which on a node with a 1S protection board is the `P−` (load) side — there the
board's cutoff switches the divider off with everything else, where wiring it
straight to the cell's own `B−` would keep drawing ~21 µA past the cutoff and
deep-discharge the pack the protection was fitted to save. And the DS18B20's
4.7 kΩ pull-up must come **off** the pin: it would drag the tap towards 3V3.

Conversions are calibrated against the chip's eFuse reference, so the driver
works in millivolts; the divider ratio is undone in
[`src/battery.rs`](src/battery.rs). That corrects the ADC, not the resistors —
use 1 % parts, and trim `R_TOP_KOHM` / `R_BOTTOM_KOHM` and reflash if a
multimeter disagrees. Published to `smarthome/terrasse/battery_voltage` in volts. Below
3.0 V the log says so: the common DW01A-class protection board does not cut off
until ~2.4 V, far past where a LiPo starts losing capacity for good.

## Enclosure

[`models.py`](models.py) is the CadQuery source for the printed parts: four
enclosures in eight pieces, plus the two bending-beam clamps the bird scale hangs
from. The clamps the file *started* as were retired in `f858d60` and are not
these — `git show d0df819:models.py` still has the originals, which put the wire
hole where the bar puts it rather than on the box's centre line.

The `wasserzaehler_*` parts are the one exception to the naming rule. Every
other part here is named after a `NODE=` slug from [`src/node.rs`](src/node.rs),
and `wasserzaehler` is not one and never will be: it is a camera tube for the
flat's two water meters, which are read by an ESP32-CAM running jomjol's
AI-on-the-edge-device, not by this crate. It lives here because this is where
the CAD toolchain lives, and a second copy of it elsewhere would be worse than
one stretched convention. The meters, the board criteria and the reasoning
behind the camera route are in `nixos-private/docs/zaehler.md`.

```bash
nix develop .#cad        # separate shell: OpenCASCADE is ~500 MB, cargo has no use for it
python models.py         # writes cad-models/*.stl and *.step
```

CadQuery is not in nixpkgs (only the `opencascade-occt` kernel, without the
Python bindings), so the shell pins the wheels and installs them into
`.venv-cad` on first entry (ignored). The exports under `cad-models/` are
tracked, so printing a part does not require the toolchain at all — `.stl` to
slice, `.step` to open in CAD.

### `terrasse_body` + `terrasse_floor` + `terrasse_charger_tray` + `terrasse_anchor`

An **88 × 78 × 71 mm** box for the bird scale, in four parts. The size is not a
choice — it comes from the measured envelopes, plugs included:

| | | | |
|---|---|---|---|
| ESP32-C3 board | 40 × 30 × 35 | HX711 board | 40 × 25 × 30 |
| battery | 65 × 50 × 10 | SHT31 | 20 × 10 × 10 |

That is 128 cm³ with clearance. The first version of this box had a 98 cm³
interior, so no amount of rearranging would have done it; a grid packing search
puts the smallest interior that takes all four at 76 × 66 × 52, and only with
everything stood on end. This one is 80 × 70 × 66, which leaves room for the
mounts themselves — and, since 2026-09-18, for a fifth part the floor could
never have held: the solar charger, on a shelf above the other two boards.

The design follows from it hanging outdoors in the rain:

- **The roof has no seam and no penetration.** The box is a cup opening
  *downward*; the only joint faces the ground, where water cannot climb to it.
  The top 5 mm flare 2 mm past the walls as an eave, so water crossing the roof
  drips clear instead of running down the sides. Nothing is drilled through the
  top — the cord attaches to two round-ended tabs *outboard* of the walls,
  which is why the part reaches 108 mm across.
- **The SHT31 lives in the −X/−Y corner, no longer behind a wall.** Air enters
  through floor slots beneath it and leaves through slots high in *both*
  adjacent walls, tilted 30° down-and-out — every opening faces down or
  outward-down, so there is still a local draught across the sensor.

  There *was* a baffle around it, and it came out because the cable could not
  be got past it during assembly: the slot was widened, then run full height,
  and it still fouled. That costs accuracy, since the board's heat now shares
  the sensor's air — about 0.9 °C on the bedroom node. A box that cannot be
  assembled is worse than one that reads half a degree warm, but keep the SHT31
  in that corner rather than beside the board, or the loss compounds.
- **The floor screws go into M3 heat-set inserts**, Ø4 bores in 9 mm posts,
  which leaves 2.5 mm of wall — thinner than one would choose, and what the
  already-printed floor plate allows.

  The screw positions are not free: they are where the 8 mm post this box
  shipped with put them, and the plate in hand has its countersinks there. So
  the post has to grow *around* a fixed hole, and it meets two features of that
  same plate — the cell reaches x = 31 and the battery guides reach y = ±26.
  Nine millimetres leaves 0.5 mm to both; ten does not fit. The plate's HX711
  rib still fouled one post by 0.5 mm, so that post is relieved there rather
  than shrunk: the rib only has to slide past, the wall has to survive brass
  being melted into it. Warm the inserts properly and do not lean on them; if
  one splits, the way out is a reprinted floor with the holes further in, which
  buys 3.5 mm.
- **The load hangs off a pad** matching the clamp's 25 × 12 face and its screw
  pitch. Two **M4 × 25 countersunk** screws run *down* from inside the box —
  heads countersunk into the nut boss, nuts in a hex pocket under the beam
  spacer, which is the end you can still reach with a spanner once the box is
  together. They ran the other way until 2026-09-18, with captive nuts in
  pockets inside the box; a nut that drops out during assembly is a nut inside
  a sealed enclosure. That pad is
  `terrasse_anchor`, its own part since 2026-09-18 and clamped under the plate
  by the same two M4 screws. It was part of the plate until then, and it was
  the only thing on that face: while it was there the plate could not be
  printed without support, because an 84 × 74 plate laid anchor-down steps out
  over a 33 × 18 flange. Now **no part of this box needs support** — body roof
  down, floor plate down, anchor pad down.
- **The solar charger sits on a tray over the two boards**, on three columns
  rising from the floor plate, and the panel lead comes in through the floor.
  Both are why the box is 71 mm tall rather than 60. See
  [`docs/solar.md`](docs/solar.md).

### `terrasse_beam_spacer` + `terrasse_beam_hanger`

The two clamps that pad exists for. The **spacer** bolts up into the floor's
captive nuts and carries the bar's fixed end; the **hanger** grips the load end,
and the wire leaves from its far end.

Two parts rather than one, because both ends of the bar share the anchor's 15 mm
screw pitch — a single pair of screws cannot fasten the box and the bar at once.
The spacer offsets the fixed end until the pairs clear. `BEAM_DIR` decides which
way the bar runs from there; flipping its sign mirrors the whole assembly, and
since every feature sits on `y = 0`, an already-printed part just turns round.

The hanger is 80 mm, the bar's own length, with the M5 bolts at one end and the
wire hole at the other, **61.5 mm apart**. The wire leaving from the far end is
a requirement — it is where the feeder hangs — and the section is dimensioned
for it rather than around it.

What follows is a cantilever in the load path, which is why the **spine** is the
deep part at 14 mm and not the pad: stiffness goes with depth cubed. Deflection
here is an accuracy problem rather than a strength one — what bends does not
spring back exactly, and that shows up as hysteresis in the weight. If readings
ever differ between a loaded and an unloaded pan, deepen `RAIL_T` before
suspecting anything in the firmware.

> **Nothing but the bar may bridge the two clamps.** Only the hanger's pad
> touches the bar; the spine runs 4 mm clear of it along its whole length and
> 16.7 mm clear of the spacer. If anything touches, the load path goes around
> the strain gauges and the scale reads a fraction of the weight — or none of
> it — without any sign that something is wrong.

**Printing.** The pad reaches down to the rail's underside rather than standing
proud of it, so there is one flat face across all 80 mm and a single 4 mm step
on top — a step *up*, overhanging nothing. The only downward faces off the bed
are the two counterbore ceilings, 112.9 mm² of bridge over a 5.3 mm hole. No
support. The pad is 18 mm thick as a result, so the bolts want to be **M5 × 16**.

Calibrate in the fixture: `scale_factor` is what absorbs whatever the mounting
does to the sensitivity.

## Wi-Fi credentials without a rebuild

A board that cannot join the network cannot be told anything over the network,
so the way in is the serial console. On a **cold boot** — power actually removed
and reapplied, not a deep-sleep wake and not a reflash — the firmware listens
briefly:

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
| `terrasse` | `smarthome/terrasse/weight`, `/temperature`, `/humidity` (SHT31-D), `/battery_voltage` — each appearing as its slot is switched on |
| `schlafzimmer` | `smarthome/schlafzimmer/co2`, `/temperature`, `/humidity` (SHT31-D), `/scd41_temperature`, `/scd41_humidity` |
| `wohnzimmer` | the same five, plus `/pm25`, `/pm10` (humidity-corrected), `/pm25_raw`, `/pm10_raw` and `/voc_index`, `/nox_index` |
| `kueche` / `bad` | `smarthome/<node>/temperature`, `/humidity` |

No node mirrors a weight to a second topic any more. The outdoor node used to,
under `birds/scale/state`, for hand-declared entities that predated discovery.
That topic and the whole `birds` namespace went when it was renamed to
`terrasse`.

**Availability.** A node that stays awake registers an MQTT last will, so the
broker publishes retained `offline` to `<namespace>/<node>/status` the moment
its connection breaks (and the node publishes `online` on connect, disconnecting
cleanly at the end of a round so a normal publish is never mistaken for a
death). Sleeping nodes get no will — they are supposed to be offline between
readings, whether that is to save a cell or to stay cool — so every node also
carries `expire_after` in its discovery config: three missed publish rounds and
Home Assistant invalidates the values.

The tuning/calibration knobs are *commands* rather than readings, but they are
discovered too — as `number`, `switch` and `button` entities in the device's
*Configuration* section — so a node needs no hand-written Home Assistant YAML at
all. See [`home-assistant/README.md`](home-assistant/README.md).

### Configure & calibrate from Home Assistant

Calibration (`offset`, `scale_factor`) and tuning (`threshold`, poll intervals)
are **stored on the controller in flash** and set from Home Assistant — no
reflashing. HA publishes each value **retained** to
`<namespace>/<node>/config/<key>` (`smarthome/terrasse/config/<key>` for the scale);
the firmware reads them the next time it is online for a publish (while a bird is
on / has just left the scale) and persists them. Changes therefore apply with a
**short delay**, not instantly.

| HA entity → topic (`smarthome/terrasse/config/…`) | Meaning | Node |
| ------------------------------------------ | ------- | ---- |
| `offset`            | raw HX711 value at 0 g (tare zero) | with load cell |
| `scale_factor`      | raw ticks per gram | with load cell |
| `threshold`         | grams that count as "a bird landed" | with load cell |
| `tare` (button)     | re-zero: takes a burst of fresh readings on the empty feeder and adopts their median as `offset` *and* as the presence baseline, and records the air temperature as the correction's anchor | with load cell |
| `temp_coeff`        | grams the zero moves per kelvin, signed; `0` disables the correction (default) | with load cell **and** SHT31 |
| `idle_interval`     | deep-sleep seconds while empty | battery |
| `active_interval`   | deep-sleep seconds for a load that outlasted its awake visit window (snow, a twig) — a normal visit is watched awake and never uses this | battery |
| `heartbeat_interval`| seconds between periodic temp + weight publishes with no visitor (default 600) | battery |
| `deep_sleep` (switch)  | `0` = stay awake with Wi-Fi up (bench testing on USB), `1` = sleep between rounds as the profile intends | any sleeping node |
| `scd41_temp_offset` | °C the SCD41 subtracts for its own self-heating (default 4.0) | with SCD41 |
| `sds011_kappa`      | κ for the PM humidity correction, `0` disables it (default 0.25) | with a compensated SDS011 |

Only the knobs that do something on a given node are announced. The three
intervals are about spending a cell, so they are battery-only: a node on a cable
samples on its build-time cadence whether or not it sleeps between rounds. The
`deep_sleep` switch follows sleeping instead, so `kueche` and `bad` have it and
`schlafzimmer` does not.
Pressing **Tarieren** publishes a retained press (the node may be asleep) which
the firmware deletes once it has re-zeroed, so it is never replayed as a second
tare. Because the press waits for the node rather than the other way round, the
re-zero is a *median* of sixteen readings taken when it wakes — a bird landing
partway through the ~1.6 s of sampling cannot become the new zero, and readings
that never settle are refused outright rather than guessed at. See
`docs/commissioning.md`.

On a blank flash the firmware falls back to built-in defaults
(`src/config.rs` — `offset` mid-scale, `scale_factor` 420, `threshold` 10 g,
2 s / 10 s idle/active intervals, 600 s heartbeat).

**Correcting the thermal zero drift (`temp_coeff`):**

An outdoor scale's zero moves with the weather, and on the terrace node it moves
far more than the load cell can account for: about **9.9 g/K**, roughly 1 % of
full scale per kelvin on a 1 kg cell and some twenty times a decent cell's own
zero-TC spec. That is the printed clamps straining against the steel beam, not
the strain gauges. Ten kelvin between a September afternoon and the following
dawn is a hundred grams — five times a blue tit.

It is not only an accuracy problem. `presence::drift_band` only absorbs creep up
to `threshold / 4`, so a thermal ramp of tens of grams lands in
`Decision::Unexplained`, where the baseline is deliberately frozen. The node then
never recovers its zero on its own; it sits there until someone tares it.

The correction subtracts `(air − tare_temp) × temp_coeff` from the **raw ticks**,
once, at the moment the sample is read — upstream of the gram conversion, the
presence comparison and the drift-tracked baseline alike, so all three keep
describing the same scale. It needs two things and does nothing without both:

1. **A coefficient.** Let the node sit through a temperature swing with nothing
   on it, then divide the published grams by the kelvin between them. Sign
   included — the terrace zero goes *down* as it cools, so its coefficient is
   negative. Enter it in the *Temperaturgang* number.
2. **An anchor.** The correction measures drift from the temperature the zero was
   taken at, and only a tare records one. So **set the coefficient first, then
   tare** — until a tare has happened under firmware that knows about the anchor,
   `tare_temp_tenths` is unset and the correction stays off however the
   coefficient reads.

Both live in the config blob beside `offset`. Flashing this firmware onto a board
that predates them keeps its existing calibration — the blob migrates from
version 5 rather than reverting to defaults — and comes up with the correction
off, so nothing changes until someone opts in.

The air comes from the node's own SHT31, read on **every** wake, including the
cheap idle ones where no other sensor is sampled: the correction has to be in
place before the presence logic looks at the sample. One I²C transaction against
a boot that costs two seconds. A round where the sensor says nothing is left
uncorrected rather than corrected against air from another hour.

One caveat the measurements so far do not settle: the SHT31 reads *air*, while
the beam and its clamps lag it. Warming and cooling branches measured 8.6 and
9.9 g/K, close enough that one coefficient is the right model, but a coefficient
fitted during a fast transient will come out too large.

**Calibrating `scale_factor`:**
1. **Tare** with the pan empty (sets `offset`). The empty-pan raw value is also
   visible in the serial monitor (`HX711 raw reading: N`).
2. Place a known mass `m` and read the published grams / raw value.
3. `scale_factor = (raw_loaded − offset) / m`; enter it in the *Kalibrierfaktor*
   number. Re-check and adjust until the reading matches.

## Long-term history

Home Assistant sees every node the moment it publishes, and keeps a recorder
database — tuned for weeks. The questions this fleet was built for are slower
than that: did insulating the roof change the bedroom's overnight CO₂, how does
the terrace swing between summers, is the feeder busier this year than last.

[`timeseries/`](timeseries/README.md) is a second consumer on the same broker
whose only job is not to lose anything. It follows the topics the nodes already
publish, writes each reading into **QuestDB** with a **three-year retention**,
maintains three cascading rollup views (`_1m` → `_1h` → `_1d`) so a year-wide
chart reads thousands of rows instead of millions, and serves a small dashboard
that routes each query to the coarsest view still fine enough to answer it.

```bash
nix develop .#timeseries
cd timeseries && cargo run -- timeseries.example.toml   # then http://127.0.0.1:8087
```

It changes nothing about the firmware or the Home Assistant side — a node does
not know it is being archived. The flake carries a package and a NixOS module
(which brings up QuestDB too, since nixpkgs ships the package but no service)
for the home server; see [`timeseries/README.md`](timeseries/README.md) for the
schema, the rollup reasoning, and how to deploy it.

## License

MIT — see [LICENSE](LICENSE).
