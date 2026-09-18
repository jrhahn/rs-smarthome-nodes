# Solar for the terrasse node — design notes

> **Panel and charger ordered 2026-09-08; nothing is built.** This is the
> design and the parts list, not a commissioning record. Every current figure it
> rests on is an estimate; see
> [Before you build any of this](#before-you-build-any-of-this) at the bottom.
> When it *is* built, the record goes in
> [`commissioning.md`](commissioning.md), not here.

## Why bother

The node's average draw is **~10 mA**, measured 2026-09-16 and three times the
3.3 mA this page used to assume:

| | measured | previously assumed |
| --- | --- | --- |
| average current | **~10 mA** | 3.3 mA |
| per day | **~240 mAh ≈ 0.96 Wh** | 79 mAh ≈ 0.3 Wh |
| on the 2000 mAh cell | **~8–9 days** | ~25 days |

**How it was measured**, since `base-platform.md` has parked every energy
question behind exactly this number: the QuestDB archiver on the family server
has been recording `battery_voltage` since 2026-09-13, and the discharge curve
is the measurement. 4.14 V → 3.70 V over 3.5 days is roughly 840 mAh of a
2000 mAh cell, i.e. ~10 mA. No bench meter, no shunt — and better than either,
because it integrates every radio burst and every visit at the real duty cycle
and the real temperature. The connect rate over those days was 143/day ≈ 6/h,
exactly what the old estimate assumed, so the radio is not where the extra went.

Eight days still is not a crisis, and the case for solar is not runtime anyway.
It is that **the box never has to be opened again**. The outdoor board has already
lost its USB port once, and every recharge is another cycle on a connector that
lives in the rain inside an enclosure whose whole shape is an argument about
keeping water out. A node that tops itself up is a node whose lid stays shut.

## The panel, and what it forces

[Waveshare 18 V / 10 W polysilicon](https://www.berrybase.de/waveshare-polysilizium-solarpanel-18v-10w-36-zellen-340x232x17-mm-20-umwandlungseffizienz-ip67),
€11.50:

Figures below are **off the panel's own label**, which differs slightly from the
shop listing (that said Vmp 17.6 V, Voc 21.6 V) — the label wins:

| | |
| --- | --- |
| Pm | 10 W (18 V × 0.55 A = 9.9 W — a self-consistent label) |
| Vmp / Imp | 18 V / 0.55 A |
| Voc / Isc | **22.3 V** / 0.6 A |
| Size, weight | 340 × 232 × 17 mm, 935 g |
| Sealing, lead | IP67, 90 cm to a 3.5 × 1.35 mm DC plug |
| Test condition | AM1.5, 1000 W/m², 25 °C |

**Voc is a winter number, not a summer one.** Silicon's voltage coefficient is
about −0.3 %/K, so cold raises it: ~24.0 V at 0 °C, ~25.3 V at −20 °C. That is
the figure every part on the charger has to survive, and it is 3 V higher than
the shop listing implied.

At ~1 peak-sun-hour a day in December and 70 % system efficiency the panel
returns ~7 Wh/day against the measured 0.96 Wh/day demand — a margin of about
**7×**. That was 23× while this page believed the 3.3 mA estimate; the real
consumption ate two thirds of the headroom and the sizing still holds
comfortably.

Two consequences follow from picking 18 V, and both are load-bearing:

- **It rules out every linear charger**, including the XIAO's own. See below.
- **It rules out the panel touching the box.** 935 g and 0.08 m² of sail area
  cannot hang off an enclosure that is suspended on a cord and whose purpose is
  weighing birds to the gram. Wind load and cable tension both tilt the box,
  and the bending beam measures what hangs below it *through* that tilt. The
  panel gets its own mount — wall, railing, post — and the cable reaches the
  box with a slack loop and no tension.

## Charger: CN3791, and why not the obvious modules

Do **not** feed the panel into the XIAO's `5V` pad. The onboard charger is a
small linear part (ETA4054-class, ~370 mA) with no input-voltage regulation: on
a current-limited source it pulls until the panel collapses, browns out, and
retries. The 21.6 V open-circuit is far outside what that pad tolerates in any
case.

The obvious shelf answer is an integrated module — Waveshare's
[Solar Power Management Module 6–24 V](https://www.berrybase.de/en/solar-power-management-modul-fuer-6v-24v-solar-panel)
(€8.80) or [Solar Power Manager D](https://www.berrybase.de/en/waveshare-solar-power-manager-modul-d-5v-3a-usb-c-fuer-6v-24v-solarpanels-ohne-batteriehalter).
**Both are disqualified by their own datasheets: quiescent current <2 mA.**
That is 60 % of the node's entire budget — the regulator would idle away more
than the node spends measuring, and it would do it all night. They also carry a
permanent 5 V boost stage this node has no use for.

A bare **CN3791** instead — a buck-topology MPPT charger, whose relevant
numbers are:

| | |
| --- | --- |
| Input | 4.5–28 V (abs. max on `VCC` 30 V) |
| Sleep drain from the cell | **≤15 µA** (`IBAT2`), i.e. 0.5 % of budget |
| Charge current | `120 mV / RCS` |
| MPPT | external divider, pin regulated to 1.205 V |
| Output | 4.2 V ±1 % |

Specifically the **Soldered MPPT Li-Ion CN3791 charger board** (SKU 333136,
€12.95), and not one of the €5 generic modules. It costs more and it is worth
it, because the hardware is open: schematic, BOM and KiCad files are
[on GitHub](https://github.com/SolderedElectronics/MPPT-Li-Ion-CN3791-charger-board-hardware-design),
which is the only reason any of the following is knowable rather than assumed.

Its listed range is "6–18 V", which describes the **MPPT adjustment range, not
the voltage rating**. That wording is a trap worth naming, because it reads like
the board cannot take this panel — it can. Read off the V1.2.1 BOM, against the
label's 22.3 V Voc rising to ~25.3 V at −20 °C:

| Ref | Part | Rating |
| --- | --- | --- |
| C3 | VKMD…1H221 electrolytic | 220 µF / 50 V |
| D3 | **AOD4185** P-channel MOSFET | −40 V, Vgs ±25 V |
| D4 | RBR5LAM30A Schottky | 30 V / 5 A |
| U1 | CN3791 | 28 V operating, 30 V absolute |

Only `D4` is anywhere near its limit — 25.3 V on a 30 V part is 84 % of rating,
in an application where it carries 100 mA and stays cold. Everything else has
room. On a no-name module none of this is documented, and the input capacitor is
the part most likely to have been chosen at 25 V.

**The alternative, and why not:** the **Youmile SD30CRMA** is the same CN3791 on
a smaller board (45 × 20 × 15 mm), sold in 9 V / 12 V / 18 V MPPT variants —
marked by hand on a printed field, not a solder bridge — and rated 18–28 V in,
which covers this panel by the vendor's own spec. `RCS` works identically. It
was rejected for one reason: **its charge voltage is a continuously adjustable
trimpot, 1.2–21 V, shipped at an arbitrary setting.** Its own documentation says
to set the output before connecting a battery. On a sealed outdoor node that is
a step that can be forgotten once and burn a cell; the Soldered board's 4.2 V is
fixed by the chip and cannot be got wrong. Smaller board, worse failure mode.

### The one change that is not optional

**`R8` must be replaced with 1.2 Ω** (1 %, ≥0.25 W).

The board ships `R8 = 40 mΩ`, which by `120 mV / RCS` is a **3 A charge
current** — 1.5 C into a 2000 mAh cell, and roughly thirty times what this node
can use. At 1.2 Ω it becomes **100 mA ≈ C/20**.

That single resistor is also what answers the cold-charging problem. A LiPo must
not be charged below 0 °C — lithium plating, permanent capacity loss, and on a
German terrace that is four months of the year rather than an edge case. But
plating is strongly rate-dependent: [Battery University](https://www.batteryuniversity.com/article/bu-410-charging-at-high-and-low-temperatures/)
puts the permitted rate at −30 °C at 0.02 C, so C/20 sits in the reduced-rate
regime that temperature-aware chargers aim for, not in the fast-charge danger
zone. And 100 mA is still ample: the 79 mAh daily budget is covered in **48
minutes** of charging.

`R8` is a **1210** part — large, and reworkable with an ordinary iron.

### Two jumpers to set while it is open

**`K2` — set the MPPT point to 18 V.** `R5` = 300 k is the fixed upper leg and
the 4×2 header selects the lower one from `R3` = 30 k, `R4` = 62 k, `R6` =
130 k, `R7` = 75 k. With the pin regulated to 1.205 V, `V = 1.205 × (300k +
Rb)/Rb`:

| Bridge | Rb | Setpoint |
| --- | --- | --- |
| R7 | 75 k | 6.0 V |
| R6 ∥ R7 | 47.6 k | 8.8 V ≈ 9 V |
| R4 ∥ R7 | 33.9 k | 11.9 V ≈ 12 V |
| **R3 ∥ R7** | **21.4 k** | **18.1 V** ← this panel |

At C/20 the panel is never loaded near its maximum power point anyway, so this
is correctness rather than yield. It is still the setting to use, and it is
something the fixed-variant generic modules cannot do at all.

**`JP1` — cut the power LED.** It is a designed-in jumper for exactly that.

The status LEDs `D1`/`D2` can stay: per the schematic they hang off `VCC`, the
**panel** side, so they cost harvest in sunshine and nothing from the cell at
night. This is the one place where the usual "desolder every LED on a cheap
module" advice does not apply — but it would have, had the board pulled them
from the battery rail.

### Why there is no NTC

The CN3791 has ten pins — `VG`, `GND`, `CHRG`, `DONE`, `COM`, `MPPT`, `BAT`,
`CSP`, `VCC`, `DRV` — and **no `TEMP` pin and no `CE` pin**. Its smaller sibling
the CN3065 does have battery temperature monitoring, but tops out at 6.5 V input
and so cannot see an 18 V panel at all. Choosing this panel means the cold-charge
question is answered by the charge current (above), optionally backed by
firmware (below), and not by the charger IC.

## Does the cold actually bind?

It feels like it should. It mostly does not, for three reasons that stack.

**Charging only happens in daylight, and daylight is the warm part of the day.**
The panel makes nothing at night or at dawn, which is when it is coldest — those
hours cost nothing because they were never productive. Charging happens roughly
10:00–15:00, at the daily temperature maximum. So the case that actually blocks
a charge is an **Eistag** (Tmax < 0 °C), not a Frosttag (Tmin < 0 °C). Frost
nights are ordinary in Germany; ice days are not. Per DWD figures the count
swings hard by winter — Berlin had 4 in the mild 2006/07 and 43 in the severe
2009/10 — and is falling across the climate reference periods. Even a severe
winter leaves well over a hundred usable days in the winter half-year.

**One usable day covers about a week.** The requirement is not one charge window
per buffer length. Consumption is 79 mAh/day; one permitted day at 100 mA over
~5 usable daylight hours returns ~500 mAh, i.e. **six days of running**. So the
node needs roughly one charge day in seven, against a 25-day buffer that is
still there underneath as the second reserve.

**And at C/20 the sub-zero prohibition is not absolute anyway** — see `R8`
above. The rate is what makes cold charging dangerous, and the rate has already
been dealt with.

If a backstop is still wanted, the firmware inhibit below should trip at
**−5 °C, not 0 °C**. It then fires on a handful of days a winter instead of
every frost day, and costs essentially no harvest.

## Wiring

```
Panel (+) ─── DC coupler ─── [ CN3791 VIN+ ]            ┌── XIAO B+
Panel (−) ──────────────────[ CN3791 VIN− ]             │
                            [ CN3791 BAT+ ]───── P+ ────┤
                            [ CN3791 GND  ]───── P− ────┴── XIAO GND
                                                  │
                                          protection board ── cell (B+/B−)
```

**The charger lands on `P+`/`P−`, not on the cell's own `B+`/`B−`.** The
protection board's overcharge FET sits in the `P−` path; wiring the charger to
the cell side puts it outside that protection entirely. `P−` is the same ground
the battery divider's foot already goes to — see
[wiring.md](wiring.md#battery-sense), which explains why
that matters for the divider too.

**The charger can go at either end**, which was not true of the first draft of
this page. The argument for keeping it indoors was that the long run should
carry the high voltage at low current — but once `R8` caps charging at 100 mA
there is no high-current side to protect: 5 m of 0.5 mm² is ~0.34 Ω there and
back, i.e. **34 mV** of drop. Put it wherever it fits, which is a question the
enclosure answers below and not an electrical one.

Nothing about the cell, the protection board or the divider changes.

### The output side is one node, so the connectors are a convenience

`K3` (JST-PH) and the `OUT±` screw terminals sit on the same `BAT` net, in
parallel with `C7` — the schematic shows them joined. So which of the two the
cell uses and which the XIAO uses is a packaging decision, not an electrical
one. The obvious split is the cell on `K3`, since a 1S pack already carries a
PH pigtail, and the XIAO on the terminals.

What is *not* free is the topology: **charger output, XIAO and the protection
board's `P` side all meet on that one node, and the cell hangs off the board's
`B+`/`B−` behind it.** Check that the pack's pigtail really comes from `P+`/`P−`
and not from the cell tabs — on a pack with the protection board taped to the
cell that is automatic, on a separately fitted board it is a wiring choice you
can get wrong.

### Input polarity is not recoverable

**There is no reverse-polarity protection on the input.** The BOM's only power
diode is `D4`, the buck's freewheel; `D1`/`D2`/`D5` are the LEDs. The input
sits on the chip. Worse, `C3` is a **polarised 220 µF electrolytic** — reverse
it and it vents.

So measure, do not infer from wire colour: panel in daylight, meter on DC volts,
red probe on the plug's **inner pin**, black on the outer sleeve. A positive
reading means the inner pin is positive, which is the usual case but not a
guarantee. Then put a piece of red tape on the positive lead before it goes into
the terminal — the measurement happens once, in daylight, with a meter in hand;
the reassembly happens later, in the dark, from memory.

### Do not cut the panel lead

The panel ships with its own IP67-moulded 90 cm lead and a DC plug. Cutting it
opens the one joint that is already properly sealed. Use the mating **DC coupler**
(part 3) and land its thin pigtail in the screw terminals.

That also buys a safety property worth having deliberately: **the panel and the
cell then end in different connectors** and cannot be swapped. Both on JST-PH
would eventually put 22 V across a 3.7 V cell, and "eventually" is a spring
evening with the box open for the third time.

## Parts

| # | Part | Spec | Source | ~Price |
| --- | --- | --- | --- | --- |
| 1 | Solar panel | Waveshare 18 V / 10 W, IP67, 3.5 × 1.35 mm DC plug | BerryBase | €11.50 |
| 2 | Charger | **Soldered MPPT Li-Ion CN3791**, SKU 333136, 54 × 38 mm | BerryBase | €12.95 |
| 3 | DC coupler | 3.5 × 1.35 mm socket with flying lead, mates with #1 | — | ~€2 |
| 4 | Extension | 2-core, ~0.5 mm², UV-resistant, length to suit the mount | — | ~€5 |
| 5 | Sense resistor | **1.2 Ω, 1 %, ≥0.25 W, 1210** — replaces `R8` | — | <€1 |
| 6 | Cable gland | M8 or PG7, with seal | — | ~€2 |
| 7 | Panel mount | bracket or clamp for 60–70° tilt, facing south | — | — |

Cell, protection board and divider are already on the node. **#1 and #2 were
ordered on 2026-09-08.**

## Mechanical

**The gland goes low in the +X wall — never the roof.** Drawn 2026-09-18:
an M8 clearance hole at z = 18, through a 4 mm pad on the inside face whose top
edge is ramped at 45° so it prints without support. The +X wall is the one that
can take it: the 9 mm between the cell lane and that wall is a clear chase from
the floor all the way up to the tray, where the other three have a board, the
vent chamber or a corner post behind them.

The reasoning it follows, which predates the drawing: The
enclosure is a cup opening downward precisely so that its only joint faces the
ground; [`models.py`](../models.py) states it as a requirement ("no seam and no
penetration in the roof"). A gland in the top would give up the one property the
whole shape exists to provide. Enter low, with a drip loop below the box.

**The board does not fit on the floor, and it does not have to.** The Soldered
charger is 54 × 38 mm, and the terrasse enclosure has no spare floor at all: the
interior is 80 × 70 × 55, and a packing search put the smallest interior taking
the existing four parts at 76 × 66 × 52. There was never room for a fifth beside
them — but there is room *above* them. The two boards stand 37 mm off an 8 mm
deck, which leaves a 13 mm band under the roof across 47 × 69 mm of plan area,
and a 54 × 38 board lies in it comfortably.

**Drawn 2026-09-18** (`models.py`, `cad-models/terrasse_charger_tray.*`):

- A **tray** over the two boards, 55 × 64.5 × 2.5, notched at the −X/−Y corner
  so the vent chimney keeps its mouth. The charger is held by two cable ties
  rather than screws — its mounting holes are not in the model because nobody
  has measured them, and ties are what the cell already uses for the same
  reason.
- **Three columns** up from the floor plate, not four. The +X/−Y corner is the
  ESP's, and the only gap there is the 2 mm between it and the cell lane; three
  points carry a 20 g board and are statically determinate. They stand in the
  pockets the existing parts leave — one 9 mm column where there is 25 mm of
  clear floor, and two 5 mm ones in gaps exactly 7 mm wide, with a millimetre
  either side. Those two get no foot: a foot is something a board has to be
  threaded past during assembly.
- Everything rises from the **floor plate**, because that is the part that
  prints anchor-down. A column standing free inside the body would begin
  printing in mid-air, the body printing roof-down — which is the same
  constraint that put every other mount on the floor.

**The box grows by 4 mm**, and only because of the charger's own height. `ENV_Z`
is now computed rather than fixed: `max(60, tray + CHARGER_H + clearance)`. At
`CHARGER_H = 9.5` or less nothing changes and only the floor plate is reprinted;
the 12 mm currently in the file is **an assumption and has to be measured on the
populated board** before anything is printed.

The alternative — a pod at the panel — stays rejected, and now for a better
reason than tidiness: it would put the *cell* at the far end of the long
outdoor cable instead of the panel. A panel is current-limited by physics
(~550 mA into a short); a LiPo is not, and relying on its protection board for a
cable that lies in the weather for years is the weaker of the two.

**Mount the panel at 60–70° from horizontal, facing south.** Steeper than the
summer optimum on purpose: the design case is December, and snow and leaves slide
off a steep panel instead of sitting on it.

## Optional: a firmware charge inhibit

If C/20 is not enough reassurance, the node is well placed to do better. The
SHT31-D already reports temperature, and on `terrasse` the pads **D3, D8 and
D10** are free ([wiring.md](wiring.md#which-pads-stay-free)). A high-side
P-channel switch in the panel line does it: AO3401A or similar (−30 V, since Voc
reaches ~23.5 V cold), gate divider 100 k/22 k against source because Vgs is
limited to ±12 V, driven by a 2N7002 from the GPIO.

**Bias the pulldown so the default is charging ON and the GPIO *inhibits*, not
the other way round.** A node whose firmware has hung and which therefore cannot
charge is the more expensive failure in January than some plating.

**Trip it at −5 °C, not 0 °C** — see
[Does the cold actually bind?](#does-the-cold-actually-bind) for why 0 °C throws
away charging days it does not need to.

## Consequences for the telemetry

Once a charger is attached, **`battery_voltage` stops being a state-of-charge
reading during daylight** — it reports the charge voltage, ~4.1–4.2 V, whatever
the cell actually holds. The meaningful sample is **shortly before sunrise**.
Rising week over week means the panel is carrying the node; falling means it is
not. Any Home Assistant template that treats the daytime value as charge state
will be wrong from the day this is fitted.

## Before you build any of this

- **The consumption figure is now measured, and the old estimate was wrong by
  3×.** See [Why bother](#why-bother). What has *not* been measured is where the
  extra 7 mA goes: the connect rate matched the old assumption exactly, so it is
  not the radio. The light-sleep floor and the amplifier's on-time are the
  candidates, and they are what to attack if eight days is not enough — not the
  publishes, which are ~5 % of the budget.
- **The divider works.** `src/battery.rs` is flashed and reporting; it is what
  produced the measurement above. It still wants trimming: the firmware read
  4.09–4.12 V against a multimeter's 4.05 V behind the BMS, which is what
  `R_TOP_KOHM` / `R_BOTTOM_KOHM` are for. Do it on battery, not on the charger
  rail, or you are calibrating against the charger.
- **Runtime knobs were changed on 2026-09-16** and the sizing above predates
  them: `idle_interval` 2 s → 5 s and `threshold` 10 g → 25 g, both retained on
  the broker. The first attacks the dominant term directly, the second stops
  wind from buying a Wi-Fi connect. Re-read the discharge curve in a few days
  before trusting the 8–9 day figure; it should improve.
