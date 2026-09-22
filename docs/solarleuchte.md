# Solarleuchte — replacing a bought garden light's electronics

> **Nothing is built and nothing is measured.** This is a diagnosis and a
> design, written 2026-09-22 from a photograph of the original board and a
> single multimeter reading. **Two bench measurements gate everything below**
> — see [Before you build any of this](#before-you-build-any-of-this). Every
> current figure for the LEDs is an assumption, and it is flagged as one each
> time it is used. When it is built, the record goes in
> [`commissioning.md`](commissioning.md), not here.

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

One thing this leaves open, and the bench test settles it: if the original
carousel changes *colour* on two wires, the module contains its own colour-cycle
IC. Such a module cannot be dimmed — PWM would restart its sequence — and would
have to be replaced after all.

## Energy: the controller can be made to disappear

The decisive point, and the reason a small panel is viable at all.

A light needs its PWM running all night, which naively means the CPU stays
awake. It does not: the ESP32-C3's **LEDC block can be clocked from the internal
8 MHz RC oscillator, which keeps running through light sleep.** So the duty is
set, the CPU light-sleeps, a timer wakes it a few seconds later to set the next
duty. The LED never flickers and the CPU is off ~99.9 % of the night.

| | awake, Wi-Fi off | light sleep + HW PWM |
| --- | --- | --- |
| controller current | ~25 mA | ~0.3 mA |
| over a 12 h night | 300 mAh | 3.6 mAh |

Both are datasheet-class estimates, not measurements. The ratio is the load-
bearing part, and it is large enough that the conclusion survives being wrong by
a factor of two.

**Wake every 5 s, not every minute.** A minute-cadence ramp steps visibly; at
5 s the wake costs nothing measurable and the fade reads as continuous.

### The budget that follows

Assuming **20 mA average LED current — an assumption, not a measurement**:

| | |
| --- | --- |
| LEDs | 240 mAh/night |
| controller | 3.6 mAh/night |
| **total** | **~244 mAh ≈ 0.90 Wh/day** |
| on the spare 2000 mAh cell | **~8 nights** |

That is the same order as the terrasse node's measured 240 mAh/day, and there
it is enough. **The cell is not the constraint.**

### The panel is

Same method as [`solar.md`](solar.md#the-panel-and-what-it-forces): December at
~1 peak-sun-hour and 70 % system efficiency, i.e. `Wh/day ≈ Wp × 0.7`.

| Panel | December yield | against 0.90 Wh/day |
| --- | --- | --- |
| Waveshare 18 V / 10 W | ~7 Wh | 7.8× — sorglos |
| 2 W | ~1.4 Wh | 1.6× — works, tight in January |
| the light's original panel (~0.5 W, **guessed**) | ~0.35 Wh | short by 2.5× |

And the same table for a controller that stays awake — 45 mA, 540 mAh, 2.0
Wh/day — turns the 2 W column into 0.7×, i.e. it does not work. **Light sleep is
what makes anything smaller than the 10 W panel viable.** That is the whole
argument for the LEDC-in-light-sleep trick, and it is why verifying it comes
first.

Measure the original panel before writing it off: open-circuit volts and
short-circuit amps in sun, product × ~0.75 is the real peak power. The 0.5 W
above is a guess from its size.

## Driving the LEDs

A GPIO cannot carry the string — the ESP32-C3 is rated ~40 mA per pin absolute
and ~20 LEDs are far past that. But the MOSFET below is a **switch**, not a
**driver**, and it does not by itself solve the second problem: the cell wanders
from 4.2 V to 3.0 V and the brightness wanders with it.

```
Cell 3.0–4.2 V ──► boost (fixed Vout) ──► LED string ──► R_set ──► MOSFET ──► GND
                        ▲ EN                                        ▲ gate
                        │                                           │
                      GPIO (off by day)                       GPIO (PWM, 2 kHz)
```

Boost to a fixed voltage, set the current with a resistor, dim with the MOSFET.
Dimming acts on the current rather than on the converter's control loop, which
is why this way round is the stable one. Set the boost **15–20 % above the
string's forward voltage**: less, and part spread plus temperature move the
current; more, and the surplus is burnt in `R_set`.

**Two traps.**

*A boost converter cannot switch its output off.* Inductor and diode leave a DC
path from input to output even with `EN` low. So the MOSFET is the switch and
`EN` is only the daytime standby — the other way round leaves the string
glowing, depending on its forward voltage.

*The module idles at ~2 mA*, which is 10 % of this budget. Drive `EN` low during
daylight and desolder the module's power LED if it has one.

**Do not** use a PT4115/AL8805-class "LED driver". They are buck topologies and
need Vin above the string voltage; from 3.7 V there is nothing to buck.

### The branch without a boost — check for it first

If the string is wired in parallel and lights at ~3.0–3.2 V, the whole converter
comes out:

```
Cell ──► LED string (with its existing resistors) ──► MOSFET ──► GND
```

Brightness then tracks the cell, and the firmware corrects for it — the node
measures `battery_voltage` anyway, so the duty is compensated against it. That
removes a stage, 2 mA of quiescent draw and a conversion loss.

The catch is the current at the top of the charge: through the same resistors,
4.2 V passes roughly 3–4× what 3.4 V does when the forward voltage sits close to
the cell voltage. Check that the LEDs tolerate the 4.2 V figure before choosing
this branch.

## Parts

| Role | Part | Note |
| --- | --- | --- |
| Controller | XIAO ESP32-C3 | on hand |
| Cell | 2000 mAh LiPo pouch | on hand, ×2 |
| Protection | BMS board | on hand |
| Charger | CN3791 | on hand; MPPT jumper to the panel, not 18 V |
| Panel | see the table above | on hand — **which one is unrecorded** |
| Boost | MT3608 module, 2–24 V | only in the boost branch |
| Switch | AO3400A | logic-level, 30 V / 5 A — overkill and fine |
| Gate | 100 Ω series, 100 kΩ to GND | the pulldown is not optional |
| Current | `R_set` | value from the bench measurement |

**The pulldown earns its line.** GPIOs float through reset and boot; without it
the lamp flashes to full brightness every time the node restarts.

A pouch cell outdoors is acceptable in a box built to be dry — the terrasse node
already does exactly that — but it has no hard can. If the light's housing can
pool water, an 18650 is the more forgiving choice.

## Firmware notes

- **Gamma-correct the duty.** A linear ramp does not look linear: it jumps at
  the bottom and flattens at the top. `duty = (perceived^2.2) * full_scale`.
- **12-bit, not 8.** After gamma correction 8 bits leave only a handful of
  distinguishable steps near black, which is where a breathing effect spends its
  time. 8 MHz / 4096 ≈ **2 kHz**, which is flicker-free to the eye and to a
  phone camera.
- **A sine half-wave over 20–30 s** reads as calm. Anything under ~10 s reads as
  agitated.
- **Night detection off the panel divider**, as the original board did — no LDR.
- **Expose the curve over MQTT** (`smarthome/solarleuchte/config/<key>`, the
  mechanism README.md already describes) so the choice between breathing and a
  flat low level is made in the garden rather than at the desk. A constant 20 %
  is a legitimate outcome and cheaper than breathing at 50 %.

Node name `solarleuchte`, following the fleet's German naming.

## Before you build any of this

- **Bench the LED module** on a current-limited supply, ~200 mA:
  1. At what voltage does it light? Under ~3.2 V → the boost-free branch is
     open. Above → a boost is needed, and the original board's two inductors
     suggest it will be.
  2. What current at a pleasant brightness? **This is the number the entire
     budget rests on, and 20 mA is currently a guess.**
  3. Does it cycle colours by itself? Then it has its own IC, cannot be dimmed,
     and has to be replaced.
- **Bench the panel**: Voc and Isc in sun. Which panel is on hand was never
  written down; the table above spans an 8:1 range of outcomes.
- **Verify LEDC survives light sleep** on the C3 with the RC clock source,
  on the desk, before the housing is closed. The entire small-panel case rests
  on it. If it does not hold, the fallbacks are an external PWM part or staying
  awake — and staying awake means the 10 W panel.
- **Count the LEDs and look for resistors on the module.** Twenty in parallel
  with individual resistors and four series groups of five behave nothing alike,
  and it is usually visible.
- The cell's **3.6 V after a sunny day** was never explained. It stops mattering
  once the board is replaced, but if the original panel is reused, its charge
  path is a suspect.
