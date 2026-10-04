# Solarleuchte — replacing a bought garden light's electronics

> **Nothing is built.** This is a diagnosis and a design, written 2026-09-22
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

One thing still open, and the bench test settles it: if the original carousel
changes *colour* on two wires, the module contains its own colour-cycle IC.
Such a module cannot be dimmed — PWM would restart its sequence — and would
have to be replaced after all.

## Who holds the PWM while the CPU sleeps

A light needs its PWM running all night, and the CPU must not be awake to
provide it. **The first version of this page proposed clocking LEDC from the
ESP32-C3's 8 MHz RC oscillator and light-sleeping between duty updates. That
does not work on this board, and this repository already knew it:**

> `Rtc::sleep_light` **resets this chip instead of resuming**, producing a boot
> loop of roughly one cycle per sleep, with no output past the last line before
> the first sleep (observed on hardware 2026-09-04).
> — [`src/main.rs`](../src/main.rs), `run_battery`

Setting `lslp_mem_inf_fpu` on a hand-built `RtcSleepConfig` did not change it,
and [`src/hx711.rs`](../src/hx711.rs) records the same finding from the other
direction: there is no sleep on this board that both retains a pad and returns.
Whatever the C3's light-sleep path needs in esp-hal 0.22, it is not available
here, and migrating off 0.22 is not a thing a garden light gets to ask for.

**So the PWM moves off-chip.** A PCA9685 — 16-channel, 12-bit, I²C, already on
the shelf — keeps its duty in its own registers and runs its own oscillator.
The ESP32-C3 then uses **deep sleep**, which this firmware already does
everywhere, and the lamp stays lit through it.

| | continuous draw |
| --- | --- |
| ESP32-C3 awake, LEDC | ~25 mA |
| ESP32-C3 deep sleep + PCA9685 | ~6 mA + wake cost |

The PCA9685's `IDD` is **6 mA typical, 10 mA max, operating mode, no load**
(datasheet, Table "Static characteristics"). Its 2.2 µA standby is not an
option at night: setting the `SLEEP` bit stops the oscillator, and *"when the
oscillator is off (Sleep mode) the LEDn outputs cannot be turned on, off or
dimmed/blinked"*. **By day it is exactly the right thing to do** — the lamp is
off anyway, so daylight hours cost 2.2 µA rather than 6 mA, and the 6 mA below
is a night-only figure.

Wake cost: ~270 ms of ROM boot and app init (measured for the terrasse node) at
~30 mA, every 5 s, is ~2 mA averaged. Stretching the wake interval is the lever
if the panel turns out small — a slow breath tolerates 15 s steps far better
than a fast one.

### The budget that follows

LED current of **20 mA is an assumption, not a measurement**, and it dominates:

| | |
| --- | --- |
| LEDs | 20 mA |
| PCA9685 | 6 mA |
| ESP32-C3 deep sleep + wakes | ~2 mA |
| **total** | **28 mA ≈ 336 mAh ≈ 1.24 Wh per night** |
| on the spare 2000 mAh cell | **~6 nights** |

### The panel decides whether this flies

Same method as [`solar.md`](solar.md#the-panel-and-what-it-forces): December at
~1 peak-sun-hour and 70 % system efficiency, i.e. `Wh/day ≈ Wp × 0.7`.

| Panel | December yield | against 1.24 Wh/day |
| --- | --- | --- |
| Waveshare 18 V / 10 W | ~7 Wh | 5.6× — comfortable |
| 2 W | ~1.4 Wh | **1.13× — no margin at all** |
| the light's original panel (~0.5 W, **guessed**) | ~0.35 Wh | short by 3.5× |

The 2 W row is the one that changed when light sleep fell away: it was 1.6×
while the controller was believed to cost 0.3 mA. **At 1.13× a single overcast
week empties the cell**, and the 6-night buffer is all there is. If the panel on
hand is in that class, either the wake interval stretches, the LED current comes
down, or the panel gets bigger.

Measure the original panel before writing it off: open-circuit volts and
short-circuit amps in sun, product × ~0.75 is the real peak power. The 0.5 W
above is a guess from its size.

## Driving the LEDs

Neither the ESP32-C3 nor the PCA9685 can carry the string. The C3 is rated
~40 mA per pin absolute; the PCA9685 sinks 25 mA typical per channel. A string
of ~20 LEDs is past both. So the PWM channel drives a **gate**, and a MOSFET
carries the current:

```
PCA9685 LED0 ──► 330 Ω ──┬──► gate  AO3400A
                         └──► 100 kΩ ──► GND
BMS P+ ──► LED string ──────► drain
                              source ──► GND
```

Since the string runs straight off the cell, that is the whole power path —
no converter, no `R_set`, no current regulation. Brightness tracks the cell
from 4.2 V down to 3.0 V, and the firmware compensates against the
`battery_voltage` the node already measures.

**330 Ω, not 100 Ω or 220 Ω.** The resistor is sized by whatever drives the
gate: the PCA9685 sources `IOH` = 10 mA, so 3.3 V / 330 Ω ≈ 10 mA stays inside
it. (An earlier draft said 100 Ω, which would have asked 33 mA of an ESP32-C3
pin rated 40 mA absolute.)

**The 100 kΩ pulldown is not optional.** GPIOs and the PCA9685's outputs float
through reset and power-up; without it the lamp flashes to full brightness on
every restart.

### Use the small MOSFET, not the fat one

Counter-intuitive, and it is the gradient that decides it. Gate charge sets the
switching time, switching time eats PWM codes, and gamma correction makes the
lowest codes the perceptually expensive ones. At 1 kHz and 12 bits one LSB is
244 ns:

| | Qg | switch time at 10 mA | codes lost | perceived brightness lost |
| --- | --- | --- | --- | --- |
| **AO3400A** (SOT-23) | ~5.6 nC | ~0.6 µs | ~2 of 4096 | ~3 % |
| IRLZ44N (TO-220) | ~48 nC | ~4.8 µs | ~20 of 4096 | ~9 % |

Datasheet arithmetic, not a measurement, but the gap is wide enough to decide
on. The fat MOSFET would make the bottom tenth of the fade mushy — which is
precisely the part this whole build exists for. A SOT-23 breakout board costs
€0.50 if hand-soldering SMD onto perfboard is unappealing.

**Do not** use a PT4115/AL8805-class "LED driver". They are buck topologies and
need Vin above the string voltage; from 3.7 V there is nothing to buck.

## Parts

| Role | Part | Note |
| --- | --- | --- |
| Controller | XIAO ESP32-C3 | on hand |
| PWM | PCA9685 breakout, 16-ch 12-bit | on hand; one channel used |
| Switch | AO3400A, SOT-23 | 30 V / 5 A — overkill and correct, see above |
| Gate | 330 Ω series, 100 kΩ to GND | |
| Cell | 2000 mAh LiPo pouch | on hand, ×2 |
| Protection | BMS board | on hand |
| Charger | CN3791 | on hand; MPPT jumper to *this* panel, not 18 V |
| Panel | see the table above | on hand — **which one is unrecorded** |

Nothing else. No boost module, no sense resistor, no gate driver — the PCA9685
output drives the gate directly.

**Two things to do to the PCA9685 breakout:** desolder its power LED, and check
its I²C pull-ups. The bus already carries the SHT31-D, so a second set of
pull-ups lands in parallel. The PCA9685's default address is 0x40 and does not
collide.

A pouch cell outdoors is acceptable in a box built to be dry — the terrasse node
already does exactly that — but it has no hard can. If the light's housing can
pool water, an 18650 is the more forgiving choice.

## Firmware notes

- **Driver crate: [`pwm-pca9685`](https://crates.io/crates/pwm-pca9685) 1.0** —
  platform-agnostic, embedded-hal, no reason to write register pokes by hand.
- **1 kHz PWM.** The PCA9685 spans 24–1526 Hz via `PRE_SCALE`; 1 kHz is
  flicker-free to the eye and leaves headroom under the ceiling. The 2 kHz an
  earlier draft specified is not reachable on this chip and was an artefact of
  the RC-oscillator plan.
- **Gamma-correct the duty.** A linear ramp does not look linear: it jumps at
  the bottom and flattens at the top. `duty = (perceived^2.2) * 4095`. The
  PCA9685's 12 bits are native, so nothing is thrown away.
- **Compensate against `battery_voltage`**, since the string runs off the cell
  and dims as it discharges.
- **`SLEEP` bit set during daylight.** 6 mA becomes 2.2 µA, and the lamp is off
  anyway. Clearing it needs 500 µs for the oscillator, then a `RESTART` —
  see the datasheet's restart sequence.
- **A sine half-wave over 20–30 s** reads as calm. Anything under ~10 s reads as
  agitated. Longer periods also cut the wake rate, which is the cheapest lever
  on the budget.
- **Night detection off the panel divider**, as the original board did — no LDR.
- **Expose the curve over MQTT** (`smarthome/solarleuchte/config/<key>`, the
  mechanism README.md already describes) so the choice between breathing and a
  flat low level is made in the garden rather than at the desk. A constant 20 %
  is a legitimate outcome and cheaper than breathing at 50 %.

Node name `solarleuchte`, following the fleet's German naming.

## Before you build any of this

- **Measure the string current.** Multimeter in series with the chain, original
  board on. **This is the number the entire budget rests on, and 20 mA is a
  guess.** Measure it again at 4.2 V from a bench supply: full charge pushes
  roughly 3–4× what 3.7 V does through the same series resistors, and if the
  LEDs dislike it the fix is a duty ceiling in firmware, not a part.
- **Check the module does not cycle colours by itself.** If it does, it has its
  own IC, cannot be dimmed, and has to be replaced.
- **Bench the panel**: Voc and Isc in sun. Which panel is on hand was never
  written down, and the table above spans an 8:1 range of outcomes. With the
  2 W row at 1.13× this is no longer an academic question.
- **Measure the PCA9685 breakout's actual draw** before trusting the 6 mA. That
  is a datasheet typical with no load, and the board carries a power LED and
  pull-ups of its own.
- **Count the LEDs and look for resistors on the module.** Twenty in parallel
  with individual resistors and four series groups of five behave nothing alike,
  and it is usually visible.
- The cell's **3.6 V after a sunny day** was never explained. It stops mattering
  once the board is replaced, but if the original panel is reused, its charge
  path is a suspect.
