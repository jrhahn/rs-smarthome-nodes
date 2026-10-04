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
firmware already does everywhere. **Verified on hardware 2026-10-04** with
[`examples/pca9685_bench.rs`](../examples/pca9685_bench.rs): channel 0 held at
2048/4096 through a 30 s deep sleep, steady, no flicker. The chip does what it
claims.

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
                               ├── CN3791 BAT+
                               └── XY-MOS VIN+ ──► OUT+ ──► string ──► OUT−
                      BMS P− ──┬── XIAO  GND
                               ├── CN3791 GND
                               └── XY-MOS VIN− and GND

   XIAO D8 (GPIO8) ──────────────► XY-MOS TRIG/PWM
                                   (100 Ω and 100 kΩ are on the module)

   XIAO D1 (GPIO3) ──► panel divider tap (ADC1)
   XIAO D2 (GPIO4) ──► battery divider tap (ADC1), as on terrasse
```

**The string hangs off `P+`, not off the XIAO.** ~150 mA has no business
crossing a microcontroller.

On the XY-MOS board the MOSFETs are in the low side: `OUT+` is tied internally
to `VIN+` and the switching happens in `OUT−`. The signal header is labelled
`TRIG/PWM` and `GND`, and both are needed — `GND` is what makes the gate
voltage mean anything.

I²C is not used. There is no PCA9685 in the final circuit — it was the fallback
for a small panel, and the panel is not small.

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
| Charger | CN3791 | on hand; `R8` → 1 Ω, MPPT jumper to 18 V |
| Panel | Waveshare 18 V / 10 W | on hand, its own — see [`solar.md`](solar.md) |

Nothing else. No boost module, no sense resistor, no PWM expander, no gate
driver.

A pouch cell outdoors is acceptable in a box built to be dry — the terrasse
node already does exactly that — but it has no hard can. If the light's housing
can pool water, an 18650 is the more forgiving choice.

## Firmware notes

The node's shape is **not** the one this firmware already has. `run_battery` is
built around cold boot, one round, deep sleep; this one wakes once at dusk and
stays up for hours. The sleeping part is the day, not the gaps.

- **LEDC, 12 bit, 1 kHz.** Native on the C3, nothing to add. See above for why
  not faster.
- **Gamma-correct the duty**, or a linear ramp jumps at the bottom and flattens
  at the top. The table in
  [`examples/pca9685_bench.rs`](../examples/pca9685_bench.rs) is the one that
  was tried and looked right on hardware 2026-10-04 — 256 entries of
  `round(((1 - cos(2πi/256))/2)^2.2 × 4095)`, whose largest neighbouring step is
  1.23 % of perceived brightness, under the ~2 % that is noticeable. It is
  independent of what drives the LEDs and carries over unchanged.
- **25 s per breath.** Under ~10 s reads as agitated.
- **Dusk from [`solar.rs`](../src/solar.rs)**, not from a light sensor: it
  already carries sunrise and sunset for 49.87 N / 8.65 E at 24 points through
  the year, worst interpolation error two minutes. The 23:00 cutoff needs the
  wall clock, which `state.rs` keeps across deep sleep and NTP anchors.
- **Compensate against `battery_voltage`**, since the string runs off the cell
  and dims as it discharges.
- **Wi-Fi off while lit.** 25 mA assumes the radio is down; a connected station
  is several times that. Connect on a schedule, not continuously.
- **Expose the curve over MQTT** (`smarthome/solarleuchte/config/<key>`, the
  mechanism README.md already describes) so the choice between breathing and a
  flat low level is made in the garden rather than at the desk.

Node name `solarleuchte`, following the fleet's German naming.

## Before you build any of this

- **Measure the string current.** Multimeter in series. It is no longer
  load-bearing — the margin covers 20 mA and 100 mA alike — but it is the one
  number on this page that is still a guess. Measure it at 4.2 V too: full
  charge pushes roughly 3–4× what 3.7 V does through the same series resistors,
  and if the LEDs dislike it the fix is a duty ceiling in firmware, not a part.
- **Check the module does not cycle colours by itself.** If it does, it has its
  own IC, cannot be dimmed, and has to be replaced.
- **Check what the XY-MOS module already has fitted**: gate series resistor,
  pulldown, and the indicator LED that has to come off.
- **Count the LEDs and look for resistors on the module.** Twenty in parallel
  with individual resistors and four series groups of five behave nothing alike,
  and it is usually visible.
- **Size the panel divider** for 18 V nominal and ~25 V `Voc` at −20 °C, not for
  the 6 V an earlier draft of this page assumed.
- The cell's **3.6 V after a sunny day** was never explained. It stops mattering
  once the board is replaced, but if the original panel is reused anywhere, its
  charge path is a suspect.
