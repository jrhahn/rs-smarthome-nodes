# Solarleuchte — replacing a bought garden light's electronics

> **Built and running** as `solarleuchte-e3c08a2`, flashed 2026-10-05. This
> page is the diagnosis and the design behind it, written 2026-09-22
> and revised the same day twice: once when the LED output was measured, and
> once when this repository's own notes killed the assumption the first version
> rested on. **The LED current is still a guess and the panel is still
> unidentified** — see
> [Before you build any of this](#before-you-build-any-of-this). Every figure
> that rests on the guess is flagged where it is used. When it is built, the
> record goes in [`commissioning.md`](commissioning.md), not here.

## What this is

A cheap outdoor solar light: small panel, one 18650, a sealed-ish plastic
housing, and an LED module on two wires. The controller board is marked
**`TC-V6.6 24-5`** — a works marking, not a part number; there is nothing
published about it.

The wanted behaviour is one mode, forever: a slow, smooth brightness gradient.
The light's own mode carousel is not wanted. The board *can* be set to a single
mode by its button, and **the setting survives days, not weeks.**

## Why the original board cannot deliver it

Three findings, in the order they were reached:

**"The program was erased" is not a failure mode here.** The firmware lives in
mask ROM or OTP on the 8-pin SOIC next to the button — a Padauk/Holtek-class
part. It cannot be erased, and it cannot be reflashed either: OTP, undocumented
die, no debug port. The controller is a throwaway part.

**The mode index lives in RAM.** There is no EEPROM on the board. Any reset
returns it to the factory carousel, which is exactly the reported symptom —
the setting is lost "after a few days", i.e. whenever the cell runs down far
enough to brown the MCU out. The board is not broken; it has no non-volatile
storage.

**A protection board does not fix it,** which is worth stating because it is the
obvious next thing to try. Protection cuts at ~2.5 V, *below* the voltage at
which the MCU has long since browned out — it arrives too late to prevent the
reset, and when it does act the disconnect is hard and the reset certain. The
cause is a missing energy margin, not missing protection, and a BMS supplies no
energy. Fit one anyway for the cell's sake; expect no change in behaviour.

### The safety question, calibrated

Asked directly, and worth recording because the answer is "not really, but":

- **Over-discharge protection exists, in firmware** — the MCU stops the boost
  around 2.8–3.0 V. It works while the firmware runs, which on this board is
  the thing in question. There is no hardware second line: no DW01 + 8205A pair
  anywhere on the PCB.
- **Charge termination at 4.2 V** is done properly by the SOT-23-6 linear
  charger next to `B+`.
- **What is missing and matters: no temperature sensing, so no cold-charge
  lockout.** The light charges at −5 °C in January, and charging a Li-ion below
  0 °C plates lithium. On a German terrace that is four months a year, and it
  is the mechanism that quietly ages these lights to death. It is the same
  problem [`solar.md`](solar.md#does-the-cold-actually-bind) answers for the
  terrasse node by capping the charge rate at C/20.
- Plus an unlacquered board in a housing that will eventually let water in.

So: an ordinary 9-€ compromise, not a hazard. The cell measured **3.6 V**, which
is mid-charge and healthy — no deep-discharge damage, nothing to dispose of.
But 3.6 V after a sunny day is *itself* a finding: a charged Li-ion should sit
at 4.1–4.2 V. Either the charge path is weak or the cell no longer holds. That
was never resolved, because the rebuild made it moot.

## The LED module decides most of the design

**Two wires means one channel.** A gradient is therefore a brightness breath,
not a colour sweep. Per-LED control was wanted and written off as impossible;
it is in fact only a question of replacing the module (WS2812 and similar carry
a controller per LED on a three-wire chain). **The decision taken was to keep
the existing LEDs** and accept one channel.

**Measured 2026-09-22: the LED output sits at 3.6–3.7 V with the output on,
i.e. at the cell voltage.** So the original board does not boost — it switches
the string straight onto the cell, and the string is parallel LEDs with series
resistors. That is ordinary for fairy lights: a white LED drops 2.8–3.2 V and
the resistor takes the rest. The two inductors on the board belong to the
charger, not to the LED side; an earlier draft of this page guessed otherwise.

Consequences: **no boost converter is needed**, and the brightness will track
the cell voltage, which the firmware corrects for (below).

**4.2 V on the string is not something this rebuild introduces.** The original
board passes the cell straight through, so the 3.6–3.7 V measured was the
cell's state of charge that afternoon and nothing else; a full cell puts 4.2 V
on the same LEDs, and has done every sunny day since the lamp was made.

What does change with charge is the current, through the drop across the
string's own series resistors. Taking a white LED's `Vf` as 3.0 V — itself
unmeasured:

| cell | drop across R | relative |
| --- | --- | --- |
| 3.4 V | 0.4 V | 1.0× |
| 3.7 V | 0.7 V | 1.8× |
| 4.2 V | 1.2 V | **3.0×** |

An earlier draft of this page said 3–4× against 3.7 V; it is 1.7× against
3.7 V, and 3× only from 3.4 V. Real diodes flatten it further, since `Vf`
climbs with current.

**Two different problems hide in that swing, and only one is free.** Mean
current, i.e. brightness drifting with the state of charge, is already handled:
the firmware compensates against `battery_voltage`, which incidentally caps the
duty at 4.2 V to whatever 3.7 V would have produced. **Peak current is not** —
during the PWM on-phase the full 4.2 V is across the string however small the
duty, so a duty ceiling cannot protect an LED from it. If the measurement shows
more than ~20 mA per LED at 4.2 V, the fix is one resistor in series with the
whole string, and it is a part rather than a line of firmware.

### Measured, and it needs a ceiling

**640 mA at 4.06 V, 25 LEDs** — from the cell with USB unplugged, 2026-10-04,
output held flat out. That is **25.6 mA per LED**, and about 29 mA at a full
4.2 V. A first attempt had read 350 mA and was clamped: the string was running
off the XIAO's own ~370 mA charger, so that number was the charger's and not
the lamp's.

The breath helps on its own — the gamma table's mean is **36 % of peak**, so
the string is never lit flat out for long. It is not enough:

| ceiling | perceived | mean | per evening | panel | evenings |
| --- | --- | --- | --- | --- | --- |
| 100 % | 100 % | 230 mA | 1660 mAh | 1.1× | 1.2 |
| 40 % | 66 % | 92 mA | 762 mAh | 2.5× | 2.6 |
| **30 %** | **58 %** | **69 mA** | **612 mAh** | **3.1×** | **3.3** |
| 20 % | 48 % | 46 mA | 462 mAh | 4.1× | 4.3 |

Unchecked, the lamp flattens a 2000 mAh cell in just over one evening and leaves
the panel 1.1× of margin. **30 % is the figure**, and it costs less than it
sounds because gamma runs the right way: energy falls with the duty, perceived
brightness only with `duty^(1/2.2)`. Three evenings of buffer and 3.1× on the
panel matches what [`solar.md`](solar.md#does-the-cold-actually-bind) already
accepts for the terrasse node — roughly one usable day in three.

Fitted in [`examples/lamp_bench.rs`](../examples/lamp_bench.rs) as
`CEILING_PCT`, applied by scaling the whole table with rounding. Relative step
sizes are untouched, so the breath is exactly as smooth as before; the only
cost is six more zeros out of 256 at the dark end, below what the eye resolves
at 1/4095.

**No series resistor.** 29 mA peak against a typical 20 mA DC rating is 1.45×,
and a datasheet normally allows two to three times the DC figure when pulsed —
which at a 30 % duty is what these are. A resistor would be redundant and would
burn the difference as heat.

**But the ceiling needs a compiled-in maximum, not only a config value.** It
belongs on MQTT so the trade can be made in the garden, and a config that could
raise it to 100 % would run the string at a DC-equivalent 29 mA for months.
MQTT may lower it; it may not raise it past ~40 %.

One thing still open, and the bench settles it: if the original carousel changes
*colour* on two wires, the module contains its own colour-cycle IC. Such a
module cannot be dimmed — PWM would restart its sequence — and would have to be
replaced after all.

## Who holds the PWM — the answer changed twice

Worth keeping in order, because the conclusion is only obvious from the end.

**First answer, wrong: LEDC through light sleep.** Clock LEDC from the C3's
8 MHz RC oscillator, light-sleep between duty updates, 0.3 mA. It does not work
on this board, and this repository already knew it:

> `Rtc::sleep_light` **resets this chip instead of resuming**, producing a boot
> loop of roughly one cycle per sleep (observed on hardware 2026-09-04).
> — [`src/main.rs`](../src/main.rs), `run_battery`

**Second answer: move the PWM off-chip.** A PCA9685 holds its duty in its own
registers and runs its own oscillator, so the C3 can use the deep sleep this
firmware already does everywhere. **Verified on hardware 2026-10-04**: channel 0 held at 2048/4096 through a 30 s
deep sleep, steady, no flicker. The chip does what it claims. The rig that
showed it is not in the tree any more — the PCA9685 left the design two
commits later — but it is in the history at `012daac`, and the gamma table it
carried lives on in [`examples/lamp_bench.rs`](../examples/lamp_bench.rs).

**Then the bench showed what that costs.** A smooth breath and deep sleep do
not fit together, and no tuning reconciles them. The fade that looked right —
25 s per breath, 256 gamma-corrected steps — wants a new duty every **98 ms**,
against a wake that costs **270 ms**. The chip cannot sleep between steps,
because it cannot come back in time to take the next one. So the breath period
is not a preference, it is bought:

| | breath | draw | per day |
| --- | --- | --- | --- |
| drift, one step per 5 s wake | ~21 min | 28 mA | 0.76 Wh |
| awake fade | 25 s | 45 mA | 1.08 Wh |

**Third and final answer: stay awake, use LEDC, and drop the PCA9685.** Which
is only affordable because of what the lamp turned out to need — see below.

### Why the burn window decides it

The lamp does not run all night. It runs from dusk to **23:00**, and at 49.87 N
December is the design case: sunset ~16:25, dark enough by ~17:00, so **6.5
hours**. June is 45 minutes.

| | |
| --- | --- |
| awake fade, 25 mA controller + 20 mA LEDs | 45 mA |
| × 6.5 h | 293 mAh ≈ **1.08 Wh/day** |
| rest of the day in deep sleep | ~0.2 mAh — nothing |
| **10 W panel, December** | **~7 Wh/day** |
| **margin** | **6.5×** |

Deep sleep stays in the design; it just moves to the hours when the lamp is
off, where LEDC dying with the digital domain costs nothing.

**6.5× is what buys the simplicity.** The panel site is not a good one, and at
this margin it does not have to be: the yield can fall to **15 % of the model**
— shading, poor aspect, dirty glass, a run of overcast days against the
6.8-night buffer on the cell — before the lamp starts losing ground. That is a
different kind of reserve from the 1.13× a 2 W panel would have given, and it
is the whole reason the PCA9685 comes back out of the circuit.

**And the lamp may be bright.** The 20 mA was a conservative guess and is no
longer load-bearing:

| LED current | per day | margin |
| --- | --- | --- |
| 20 mA | 1.08 Wh | 6.5× |
| 50 mA | 1.80 Wh | 3.9× |
| 100 mA | 2.53 Wh | 2.8× |

100 mA continuous through the string is still carried threefold. Brightness is
a matter of taste here, not of budget.

**The panel is a second one**, bought for this lamp; the terrasse node keeps
the one [`solar.md`](solar.md) allocates to it.

## Driving the LEDs

A GPIO cannot carry the string — the C3 is rated ~40 mA per pin absolute and
~20 LEDs are past that. So the PWM pin drives a **gate**, and a MOSFET carries
the current:

```
GPIO ──► TRIG/PWM ──► [ 100 Ω ──┬── gates  Q1 ∥ Q2 (D4184) ]   on-module
                                └── 100 kΩ ──► GND
BMS P+ ──► VIN+ ──► OUT+ ──► LED string ──► OUT− ──► drains
BMS P− ──► VIN− and GND
```

Since the string runs straight off the cell, that is the whole power path — no
converter, no sense resistor, no current regulation. Brightness tracks the cell
from 4.2 V down to 3.0 V, and the firmware compensates against the
`battery_voltage` the node already measures.

**Both resistors are already on the XY-MOS board, and nothing needs adding** —
read off the photographed module 2026-10-04:

| Ref | Code | Value | Role |
| --- | --- | --- | --- |
| `R3` | `101` | **100 Ω** | gate series |
| `R1` | `104` | **100 kΩ** | gate pulldown |
| `R2` | `202` | 2 kΩ | indicator LED |

100 Ω rather than the 220 Ω this page specified, and that turns out to be the
right value here for a reason the earlier draft could not know: **the board
carries two D4184 in parallel**, `Q1` and `Q2`, so the gate charge is doubled to
~27 nC. The lower resistor compensates — peak gate current is
3.3 V / (100 Ω + ~30 Ω of pin impedance) ≈ **25 mA**, inside the C3's 40 mA
absolute rating, and the switching time lands at ~0.8 µs, i.e. ~4 of 4096 codes.
Adding a series resistor would only slow it down.

**Leave the indicator LED alone too.** An earlier draft said to desolder it on
a guess of 1–3 mA; at 2 kΩ it draws ~0.65 mA, which is 4 mAh over an evening
against a 293 mAh budget. Not worth the iron.

The pulldown is the part that matters at boot: GPIOs float through reset, and
without `R1` the lamp would flash to full brightness on every restart.

### Gate charge decides the bottom of the fade

The MOSFET is chosen by its gate, not its current rating — anything here
carries 150 mA without noticing. Gate charge sets the switching time, switching
time eats PWM codes, and gamma correction makes the lowest codes the
perceptually expensive ones. At 1 kHz and 12 bits one LSB is 244 ns:

| | Qg at 4.5 V | switch time at 15 mA | codes lost | perceived |
| --- | --- | --- | --- | --- |
| AO3400A (SOT-23) | ~5.6 nC | ~0.4 µs | ~2 of 4096 | ~3 % |
| **D4184 ×2** (XY-MOS module) | ~27 nC | ~0.8 µs | ~4 of 4096 | ~4 % |
| IRLZ44N (TO-220) | ~48 nC | ~3.2 µs | ~13 of 4096 | ~7 % |

Datasheet arithmetic, not a measurement. **The D4184 is good enough and already
on the shelf**, which settles it — a modern trench part, not the TO-220 class
the third row stands for.

**The caveat is the threshold, not the charge.** `VGS(th)` is 2.2 V typical and
**2.6 V maximum** against a 3.3 V gate, so worst case leaves 0.7 V of overdrive,
and `RDS(on)` is characterised only at 4.5 V and 10 V. Conduction stays
irrelevant at 150 mA — even 100 mΩ is 15 mV — but the crossing of the threshold
is slow and varies between parts, and it is slow exactly where the eye is
looking. Build it, then watch the bottom of the breath; an **AO3400A**
(`VGS(th)` 0.65–1.45 V) is the ten-cent fix if it sticks.

**Do not raise the PWM frequency to "improve" things.** It makes this worse:
the LSB shrinks with frequency while the switching time does not, so 4 kHz
would cost four times the codes at the bottom. 1 kHz is the figure.

**Do not** use a PT4115/AL8805-class "LED driver" either. They are buck
topologies and need Vin above the string voltage; from 3.7 V there is nothing
to buck.

## Wiring

Drawn: [`solarleuchte-wiring.pdf`](solarleuchte-wiring.pdf) (A4 landscape, for
the bench), source [`solarleuchte-wiring.svg`](solarleuchte-wiring.svg). Edit
the SVG and re-export with
`inkscape --export-type=pdf --export-filename=docs/solarleuchte-wiring.pdf docs/solarleuchte-wiring.svg`.

```
                      BMS P+ ──┬── XIAO  B+
                               ├── charger BAT+
                               ├── XY-MOS VIN+ ──► OUT+ ──► string ──► OUT−
                               └── 100 kΩ ──┬── XIAO D2 (GPIO4)
                                            ├── 100 kΩ ──► P−
                                            └── 100 nF ──► P−
                      BMS P− ──┬── XIAO  GND
                               ├── charger GND
                               └── XY-MOS VIN− and GND

   XIAO D8 (GPIO8) ──────────────► XY-MOS TRIG/PWM
                                   (100 Ω and 100 kΩ are on the module)

   XIAO 3V3 ─────────────────────► SHT31-D VCC
   XIAO D4 (GPIO6) ──────────────► SHT31-D SDA
   XIAO D5 (GPIO7) ──────────────► SHT31-D SCL
                                   SHT31-D GND ──► P−
```

| Pad | GPIO | What |
| --- | --- | --- |
| `D2` | 4 | battery divider tap (ADC1) |
| `D4` | 6 | I²C SDA — SHT31-D |
| `D5` | 7 | I²C SCL — SHT31-D |
| `D8` | 8 | LEDC → `TRIG/PWM` |

```
   XIAO D2 (GPIO4) ──► battery divider tap (ADC1), as on terrasse
```

**The string hangs off `P+`, not off the XIAO.** ~150 mA has no business
crossing a microcontroller.

On the XY-MOS board the MOSFETs are in the low side: `OUT+` is tied internally
to `VIN+` and the switching happens in `OUT−`. The signal header is labelled
`TRIG/PWM` and `GND`, and both are needed — `GND` is what makes the gate
voltage mean anything.

There is no PCA9685 in the final circuit — it was the fallback for a small
panel, and the panel is not small. I²C carries only the thermometer.

### The charger has a trimpot, and it is the dangerous kind

Not the Soldered CN3791 [`solar.md`](solar.md#charger-cn3791-and-why-not-the-obvious-modules)
specifies for terrasse, but the class of module that page *rejected*: an 18 V
MPPT variant whose **charge voltage is a continuously adjustable trimpot,
shipped at an arbitrary setting**. Its own documentation says to set the output
before connecting a battery, and that warning is the whole reason terrasse got
a board whose 4.2 V is fixed by the chip.

So, in this order and no other:

1. **No cell connected.** Panel on `VIN`/`GND`, in daylight.
2. Meter on `BAT+` to `GND`, turn the pot to **4.20 V**. Not 3.6 V — that is
   LiFePO4, and this cell is not.
3. Only then connect the cell.

Verified on the board in hand: the electrolytics are **47 µF / 63 V**, against
22.3 V `Voc` rising to ~25.3 V at −20 °C. Ample, and the one thing about this
module that needed no attention.

**Done 2026-10-08.** Pot set to **4.20 V** at `BAT+` with no cell connected,
panel reading 22.8 V on the input — above the label's 22.3 V `Voc`, which is
what a cool October day does to it, and a second confirmation that 63 V of
capacitor is not close to anything. Partly cloudy at the time, which does not
matter: above the regulation point the output does not follow the irradiance.

**Seal the screw, and not with threadlocker.** The adjuster is part of the
wiper, not a fastener — anaerobic threadlocker and thin cyanoacrylate both
creep into the element, where they shift the resistance or seize the wiper so
it tears on the next adjustment. A drop of lacquer (plumbing seal, torque seal,
nail polish) on the outside edge between screw head and body is the usual
answer, and it doubles as a tamper indicator: a contrasting colour against this
dark board shows a cracked seal at a glance. Let it cure for hours rather than
minutes before closing the enclosure — solvent vapour in a sealed box with
electronics buys nothing — and **re-measure afterwards**, since lacquer shrinks
as it dries and can drag the wiper a few millivolts with it.

### Which hands us the divider calibration

[What is still open](#what-is-still-open) said the divider could not be trimmed
because the only available reference — the charger's own termination — carried
the same ±1 % uncertainty as the error being measured. That is no longer true.
**The termination is now set to 4.20 V against a multimeter**, deliberately,
and the firmware's reading of the same voltage is the comparison.

So let the cell charge full on a sunny day and read the plateau off
`smarthome/solarleuchte/battery_voltage`:

| Plateau | What it means |
| --- | --- |
| 4.20 V | the divider is right; nothing to trim |
| ~4.16 V | reads ~40 mV low, which is what the 2026-10-05 reading suggested |
| above 4.25 V | the *pot* is too high, not the divider — turn it down |

The caveat from that section still stands: `R_TOP_KOHM` and `R_BOTTOM_KOHM` are
one pair for the whole fleet, so a trim here moves terrasse too.

### `RCS` ships at 0.12 Ω, and 1 Ω is the wrong answer here

`120 mV / RCS`, the same rule as terrasse's `R8`. The board carries `R120` —
0.120 Ω in the decimal-point notation, confirmed by measurement once the
meter's own 0.4 Ω of leads was subtracted. That is **1 A**, or 0.5 C into this
cell.

The terrasse value does not transfer, and the reason is the load: that node
draws 240 mAh a day, this one 650 mAh an evening.

| `RCS` | current | December harvest | limited by | margin | rate |
| --- | --- | --- | --- | --- | --- |
| 0.12 Ω (as shipped) | 1000 mA | 1890 mAh | the panel | 2.9× | C/2 |
| **0.5 Ω** | **240 mA** | **1680 mAh** | `RCS` | **2.6×** | **C/8** |
| 1.0 Ω (terrasse's) | 120 mA | 840 mAh | `RCS` | 1.3× | C/17 |

Above 0.33 Ω the panel is the limit rather than the resistor, so **0.5 Ω costs
11 % of the harvest and halves the charge rate** — which is the trade worth
making, because rate is what makes sub-zero charging dangerous. At 1 Ω the lamp
would run out in November.

**Not urgent, and dated.** The risk is charging below freezing, which starts in
December; until then 1 A is simply the better harvest. Fit it before the first
frost.

### Why there is a thermometer on a lamp

Nothing here reads the weather. The SHT31-D is on `D4`/`D5` to answer one
question nobody has measured: **is this cell ever actually charged below
freezing?**

Two things argue it is not, and both are currently assumption. Charging happens
at midday, the warmest part of the day — the case that blocks is an *Eistag*,
not a frost night. And the cell sits in a dark enclosure in the sun during
exactly those hours, so it can be above zero while the air is at −3 °C.

**What was considered and not built: a firmware charge inhibit.** The circuit is
already drawn in [`solar.md`](solar.md#optional-a-firmware-charge-inhibit) — a
P-channel high-side switch in the panel line. It is much weaker here than
there, because **this node is asleep for the whole charging window**. Making it
work would mean latching the pin through deep sleep with `RTC_CNTL.PAD_HOLD`,
the way `park_scale` holds the HX711's, plus waking hourly through the day to
re-evaluate. And that inverts the safety property that page insists on: with a
latched pad the default is no longer "charging on" but "whatever was last
written", so a firmware that hangs while inhibiting leaves a cell that never
charges again.

Measure first. A winter of temperature data costs one part and no firmware; an
inhibit built for an unmeasured problem costs a new way to kill the node.

### The battery divider is not telemetry here

Fitted 2026-10-04, same parts and same pin as the terrasse node, which
[`README.md`](../README.md#wiring-battery-divider--xiao-esp32-c3) already
describes: 100 kΩ / 100 kΩ from the battery rail to ground, 100 nF across the
lower leg, tap on `D2` (GPIO4, ADC1). `src/battery.rs` undoes the ratio and
calibrates against the chip's eFuse reference, so there is nothing to write.

**The foot goes to `P−`, not to the cell's `B−`.** On terrasse that rule saves
~21 µA from drawing past the protection board's cutoff and deep-discharging the
pack the board was fitted to protect; it is the same rule here.

What differs from terrasse is the *purpose*. There `battery_voltage` is
telemetry. Here it is **part of the control loop**: the breath is compensated
against it, or the brightness drifts with the state of charge over the evening,
and the duty ceiling below is expressed against it too. No divider, no ceiling.

### Before powering it up

The list in [`wiring.md`](wiring.md#before-you-power-it-up) applies.

1. Continuity from every module's GND to the XIAO's GND.
2. Nothing to fit: `R3` and `R1` are already on the XY-MOS board.
3. Nothing on `D9` — a wire there strapped the chip into download mode for an
   evening on 2026-10-04, and the symptom was a board that looked dead.

## Parts

| Role | Part | Note |
| --- | --- | --- |
| Controller | XIAO ESP32-C3 | on hand |
| Switch | XY-MOS module, D4184 | on hand; see the gate-charge section |
| Gate | 100 Ω series, 100 kΩ to GND | **already on the module** (`R3`, `R1`) |
| Fallback switch | AO3400A, SOT-23 | ~€0.10, buy two against a mushy fade |
| Cell | 2000 mAh LiPo pouch | on hand, ×2 |
| Protection | BMS board | on hand |
| Charger | MPPT module, 18 V variant | on hand; **set the pot, swap `RCS`** — below |
| Thermometer | SHT31-D breakout | on hand; `D4`/`D5`, telemetry only |
| Panel | Waveshare 18 V / 10 W | on hand, its own — see [`solar.md`](solar.md) |

Nothing else. No boost module, no sense resistor, no PWM expander, no gate
driver.

A pouch cell outdoors is acceptable in a box built to be dry — the terrasse
node already does exactly that — but it has no hard can. If the light's housing
can pool water, an 18650 is the more forgiving choice.

## The firmware

Built 2026-10-05 and running as `solarleuchte-e3c08a2`. Three pieces, and the
split between them is the design:

| | |
| --- | --- |
| [`src/lamp.rs`](../src/lamp.rs) | the whole brightness rule, as pure arithmetic |
| `lamp_task` in [`main.rs`](../src/main.rs) | writes a duty every 97 ms and does nothing else |
| `run_lamp` in [`main.rs`](../src/main.rs) | the evening: clock, cell, network, sleep |

**`lamp.rs` has no HAL, no floats and no clock of its own** — a duty is a
function of the minute, the day and the cell voltage, which is what makes it
testable on the host. Twenty tests cover the window edges, the clamps and the
monotonicity.

### The duty is a product of four things

```
duty = BREATH[step] × MAX_DUTY_PCT × twilight × charge × (HA brightness)
```

- **`BREATH`** is the shape: the gamma-corrected half-cosine above, carried
  over unchanged from the bench rig since it never depended on what drove the
  LEDs.
- **`MAX_DUTY_PCT` = 70 %**, compiled in and reachable by nothing. Home
  Assistant's slider is a factor *on* it: 255 means "as bright as the ceiling
  allows". A config able to reach 100 % would run 25 LEDs at a DC-equivalent
  29 mA for months.
- **`twilight_permille`** opens the window 45 minutes after sunset, ramps over
  40, and fades out over the last 10 before the cutoff. An abrupt cut reads as
  a failure.
- **`charge_permille`** is the entire weather model. Aspect, shading and last
  week's cloud all land in the cell, so reading the cell beats predicting any
  of them — and it dims through a dark week instead of going out in the middle
  of one.

`lamp::window` is the single definition of when the lamp is lit, and a test
asserts the direction that can strand a node: **nothing outside it is ever
bright.** That test found a real off-by-one at the start minute on its first
run.

### Why the breath has its own task

It was not built that way, and the first version was visibly jerky in the
garden. The fade shared a loop with a battery conversion — which discards one
sample and averages several more — so every step came out 97 ms *plus* an
unpredictable ADC, and every five minutes a publish froze the string for
seconds while MQTT talked.

`lamp_task` now owns the channel and does nothing but write a duty and wait.
`run_lamp` recomputes the gate every 30 s and hands it over through a `Signal`,
newest value wins. `lamp.rs` splits to match — `gate_permille` for what moves
in seconds, `shaped` for what moves in milliseconds — with a test that the two
compose back into `duty`, so the split cannot drift.

### Wi-Fi stays up, which the plan did not say

An earlier draft of this page said "Wi-Fi off while lit". It is not, and cannot
easily be: `bring_up_wifi` consumes the radio peripherals and runs once per
boot, so an evening gets one association or none.

What that buys is live control — every publish round returns the retained
config, so a slider moved at nine is obeyed by five past. What it costs is a
connected station in `modem sleep: max` for the evening, a few mA against the
string's mean of ~69. The 3.1× margin above becomes roughly 3.0×.

### Fail-safety

The lamp is the first node here that stays awake for hours, which breaks
assumptions the rest of the fleet never had to state. Found by review on
2026-10-05, after two of them had already shipped:

| Failure | What it did | What it does |
| --- | --- | --- |
| Router reboots | slept an hour — a dark router meant a dark garden | breathes unattended on the clock, publishes nothing |
| Boot outside the window | idled awake with Wi-Fi until 22:00 UTC, ~420 mAh | sleeps at both ends of its window |
| Broker down 15 min | rolled back a good image: an attempt per publish against `MAX_ATTEMPTS` of 3 | one attempt per boot |
| Publish hangs | string lit all night; the watchdog feeds from its own task and `lamp_task` kept breathing | 20 s budget, same as `connect_and_publish` |
| Cell below 3.73 V | dark, but still awake and talking: ~180 mAh of what was left | sleeps until the next dusk |
| Image genuinely broken | three evenings to roll back, one attempt per daily boot | 30 minutes to reach the broker, then restart |

**The one that nearly stranded it**: `run_lamp` called `publish_samples`
directly, and the three things `connect_and_publish` does around it are not
decoration. Without `install_if_offered` an offer is picked up and never acted
on, and without `ota_begin_attempt`/`ota_confirm` an image that does arrive is
never marked good. `d9ff3e9` was therefore a firmware that could only be
replaced with a cable, on a node whose whole point is hanging in a garden. It
was found by trying to update it.

Checked and left alone: the watchdog (fed from its own task, so it feeds
however long `run_lamp` blocks — which is *why* the publish needed its own
budget); Wi-Fi reconnect (`StaDisconnected` with a 5 s backoff, so an AP reboot
heals itself); `D8` floating in deep sleep and through boot, where the XY-MOS
100 kΩ pulldown holds the gate down; NTP implausibility; a silent ADC, which
falls back to 3700 mV rather than to the brightest evening the ceiling allows.

### Home Assistant

A `light` entity, not a slider and a switch beside each other — `components`
gained `light` for it, and `ent_cat` is now derived from the component, since a
knob is configuration and a lamp is the thing the device *is*. The brightness
pair rides in `Control::spec`, which is free-form JSON, so no renderer changed.

Neither `enabled` nor `brightness` is in the config blob. The retained MQTT
topics already are the store, and the gap a second copy would cover — cold boot
to first connect — is one where the ceiling and the charge factor are both
still in force.

## What is still open

Everything this page once listed as "before you build" has been built. Two
things are genuinely unfinished, and one of them is only unfinished because
nobody has held a multimeter to it.

**The battery divider is uncalibrated, and it now matters more than it did.**
It reads and it is stable — 4160 mV once the charger had finished, moving 8 mV
between idle and 640 mA, which also puts the cell's internal resistance at
~23 mΩ and healthy. But the charger's own termination is only nominally
4200 mV, ±1 %, so the ~40 mV gap is the same size as the reference's
uncertainty and cannot be trimmed against it. A multimeter at the cell settles
it in ten seconds.

Why it matters here and not on terrasse: there `battery_voltage` is telemetry,
and here [`lamp::charge_permille`](../src/lamp.rs) derives the evening's
brightness ceiling from it. 40 mV is about four points of state of charge, so
the ceiling sits ~6 % high — and it does that in exactly the conditions where
the margin is thin.

**`R_TOP_KOHM` and `R_BOTTOM_KOHM` are one pair for the whole fleet.** Trimming
them for this lamp moves terrasse's reading too, whose divider is different
resistors and which read *high* rather than low. A per-node calibration would
need fields on `NodeConfig` and a `cell_millivolts` that is no longer a `const
fn`. Worth doing once there are two measured dividers to calibrate, not before.

### Settled on the way

- **The string has no colour-cycle IC.** It dims cleanly under PWM, which a
  module with its own controller could not.
- **The XY-MOS carries `R3` 100 Ω and `R1` 100 kΩ.** Nothing had to be fitted,
  and its indicator LED draws 0.65 mA, which is not worth an iron.
- **No panel divider.** An earlier draft reserved `D1` for one;
  [`solar.rs`](../src/solar.rs) gives dusk from the calendar and the pin stayed
  free.
- The cell's **3.6 V after a sunny day** is explained: the lamp's own charger
  was holding it mid-charge against a continuous load, not finishing. It
  stopped mattering when the board was replaced.
