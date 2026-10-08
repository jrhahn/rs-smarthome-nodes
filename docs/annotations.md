# What happened to the fleet, and when

A dated list of everything that changed what the numbers *mean* — a sensor
replaced, a counter reset, a cadence changed, a gap and its cause. The readings
themselves are in QuestDB; this is the part that cannot be derived from them.

Why bother: in two years a step in a series is indistinguishable from a real
change in the house. The terrace's weight jumped on 2026-09-09 because the scale
was recalibrated, not because a heavier bird arrived, and nothing in the data
says so. [`long-term-history.md`](long-term-history.md) argues the case at
length; this file is the cheap half of it.

**The `annotations` table holds the same list, and the two are not rivals.**
This file is where an event gets the paragraphs it needs — the evidence, what was
ruled out, what it does to which channel. The table adds *where*: a note with a
timestamp is drawn on the chart it explains, at the point it explains, which is
where somebody actually asks. So an event worth a note gets both, and the short
version in the table ends by pointing here. See
[`questdb/annotations.rs`](../timeseries/src/questdb/annotations.rs) for the
table itself, and note that its API needs a `from`/`to` range — without one a
query comes back empty rather than unfiltered.

**One entry per event.** Date, node, channel if it matters, what happened, and
what it does to the data. Newest last, so the file reads forwards.

---

## 2026-09-09 — terrasse — the night the cell flapped

An uncalibrated load cell sat 21152 ticks over a 4200-tick threshold and
reported an arrival every active round for eleven hours, at ~230 broker sessions
an hour against a design rate of six. It flattened a 2000 mAh pack. Everything
`terrasse` published that night is the scale arguing with itself; the visit
counter did not exist yet, so only `weight` is affected.

Fixed by the publish rate limit (`presence::MIN_PUBLISH_GAP_SECS`) and a
recalibration.

## 2026-09-12 ~09:37 — terrasse — `visits` reset from garbage

The visit counter came up reading **2,345,324,652** — uninitialised RTC RAM,
which a reflash keeps. The value is visible in Home Assistant's history up to
this point and is not a count of anything. After the fix (`36b5e36`, a
checked pair in RTC RAM) and a reflash it restarts at 0.

So: `terrasse/visits` before 2026-09-12 is meaningless, and the daily meter's
figure for 2026-09-12 (193) covers ten hours, not a day.

## 2026-09-12 19:18 — wohnzimmer — SGP41 fitted

`voc_index` and `nox_index` begin here. Both are **relative** numbers: 100 is
the running average of roughly the last 24 hours in that room, so the first
day's values climb from ~1 as Sensirion's algorithm learns the room rather than
because the air improves. Indoors `nox_index` sits on its floor unless something
burns.

## 2026-09-12 ~20:00 – 2026-09-13 09:15 — wohnzimmer — VOC/NOx gap

A faulty 10 cm jumper on the SGP41's supply. The part acknowledged its address
and its commands, then fell silent mid-measurement — the 50 mA hotplate pulse
was enough to drop it out over a marginal contact. Diagnosed only after the
driver learned to say *which* way it failed (`afb34db`); replacing the cable
brought it straight back, with the same serial number as on the first day, so
the sensor was never at fault.

No VOC or NOx data in this window. Nothing else on that node is affected —
`temperature`, `humidity`, `co2` and the particulates ran throughout.

## 2026-09-13 ~08:20–08:45 — wohnzimmer — short `temperature`/`humidity` gap

Self-inflicted while diagnosing the above: `espflash monitor` restarts the MCU
without cutting the sensors' 3V3, and an SHT31-D left part-way through a command
answers its address for ever after while refusing every measurement. Three
rounds are missing. A power cycle cleared it, and the driver now retries behind
a soft reset so the same restart costs at most one late round.

## 2026-09-13 08:05 — the whole fleet — the archive starts here

`smarthome-timeseries` went live on family-server. **There is no QuestDB history
before this timestamp**, for any node. Anything older exists only in Home
Assistant's recorder, which keeps about ten days — so by late September 2026 the
period before this line is gone for good.

## 2026-09-13 08:31 — kueche, bad — cadence and power profile changed

Both nodes moved to `PowerProfile::MainsDutyCycled`: they now deep-sleep between
readings instead of staying associated, because a board idling at 80–110 mA
inside a 65 × 39 × 30 mm box warms the air its own SHT31 is measuring.

Two consequences for the data. Their readings should read slightly **cooler**
from here, and that step is the box cooling down, not the room. And they are
unreachable between rounds, so their `sample_secs = 120` is now also the
resolution of anything they publish.

**This is when the code changed, not when the boards did.** A node keeps running
what it was last flashed with. `bad` was flashed that day and runs the profile;
`kueche` did not get it until the evening — see its entry below.

## 2026-09-13 20:26 — kueche — duty-cycled firmware actually flashed

The profile changed in the source at 08:31; the board got it now. So the step
the entry above predicts — slightly cooler readings, from the box no longer
warming its own sensor — starts *here* for `kueche`, twelve hours later. Before
this line it was still a board idling at 80–110 mA next to its own SHT31.

Flashed from the laptop over USB, identified by MAC `ac:27:6e:82:43:94` rather
than by which cable was in hand. Confirmed from the boot log rather than from
the flash succeeding:

    node 'kueche' (Küche) booted, mains, duty-cycled profile
    wifi: 'wifi_42_ext' (built in)
    SHT31-D found at 0x44

and from the cycle itself — awake 20:28:11, published 20:28:17, asleep again by
20:28:34. Roughly 23 seconds of every 120.

Worth writing down about this board specifically: **nothing is stored in its
flash.** Sectors `0x9000`/`0xA000`/`0xB000` — calibration, node identity, Wi-Fi
— all read back blank, so it runs entirely on what it was compiled with. Read
those back *before* reflashing a node, not after: a board whose credentials live
only in the image goes off the network for good if it is rebuilt without
`SSID=`/`PASSWORD=`, and it cannot be told about it afterwards.

## 2026-09-13 11:54–20:55 — bad — SHT31-D silent, and it was a broken wire

`bad` stopped publishing `temperature` and `humidity` at 09:54:15Z. It did not
stop publishing: `rssi` continues on the same 120 s cadence, so the node boots,
joins, and reports — it simply has no sensor to read. **Everything after this
timestamp is missing, not zero**, and the node's own availability says nothing
about it, because a node with a dead sensor is still online.

The firmware says which half is broken, and it is not the chip:

    no SHT31-D at 0x44 or 0x45
    I²C scan: nothing answered between 0x08 and 0x77 — the bus itself is not
    working (SDA/SCL swapped, no pull-ups, or no power)

A full scan with no reply at any address rules the sensor out. One dead chip
leaves the bus usable; silence across all 112 addresses is power or wiring.

The timing points the same way. The new printed box went in that morning, and in
that design the jumpers to the SHT31 loop through the 8.5 mm between board and
baffle — the corridor `models.py` deliberately keeps clear of vent slots. A wire
pulled or pinched during assembly fits the evidence exactly; a chip that died
three hours after being handled does not.

Not a firmware problem: the board is on `MainsDutyCycled`, joins as
192.168.1.143 at −53 dBm, and publishes. It needed a look inside the box.

**Resolved at 18:55:16Z: one of the jumpers had broken off.** Readings resume on
the next round, so the gap is 09:54:15Z to 18:55:16Z — nine hours and one
minute with no `temperature` or `humidity` from this node, and `rssi` throughout.

The first two rounds back read 27.9 °C / 63.4 % then 27.4 °C / 58.0 %. Falling,
because the box was open and being worked on; treat the first few minutes after
this line as the sensor settling, not as the bathroom.

What is worth keeping from this: the fault was localised without opening
anything. A full I²C scan answering nowhere said "not the chip", and the clock
said "whatever was touched this morning". Both were right, and the scan line is
in the boot log of every node, every boot.

## 2026-09-13 20:43–22:23 — kueche — moved about, and then a USB-A port

The node was unplugged from the kitchen at 20:43:24 for the reflash and did not
publish reliably again until 22:23:10. Two separate things to know about that
window, and the second is the one that will mislead somebody.

**The gap is not empty.** `kueche` published from the laptop several times while
it was being worked on — 22:07, 22:09, and a continuous run from 22:37 to 22:58
— and those rows look exactly like kitchen readings. They are not. They are a
board in another room, being carried around, in an open box. Anything from
`kueche` between these two timestamps is the node's own air, not the kitchen's.

**Why it would not stay up in the kitchen.** It came back after each plug-in,
published exactly one round, and went silent. Always exactly one, which is not
what a loose connection looks like. The board itself was fine: on a laptop USB
port it ran ten consecutive wakes at 125 s, and `bad` — same firmware, same
`MainsDutyCycled`, same 120 s sleep — never missed one all evening.

The kitchen's socket strip has a USB-A and a USB-C port, and the node works on
the C and dies on the A. That difference is the whole explanation. A USB-C
source decides that something is attached by measuring the sink's 5.1 kΩ on CC —
a static resistance, true whether the device draws an amp or nothing. A USB-A
port has no such line and no other way to tell "asleep" from "unplugged", so it
watches the current and shuts down below its threshold; standby-power rules
require it to. In deep sleep the ESP32-C3 draws tens of microamps. The port
switched off, and a board with no rail never wakes — hence one round per
plug-in, and hence the red power LED going out, which is a 3V3 LED the firmware
never touches and deep sleep never dims.

**The rule: a duty-cycled node goes on USB-C or on a dumb supply, never on a
USB-A charging port.** This became possible only this morning. As a plain mains
node `kueche` stayed awake and drew enough to hold the port up; the duty cycle
created the idle it switches off in. Nothing was broken — the firmware is right
and the strip is right, they just cannot be combined. **`bad` runs the same
profile; check what it is plugged into before this happens there too.**

## 2026-09-14 04:19–19:46 — kueche — dead again, and the USB-C port was not the fix

Fifteen and a half hours with nothing from this node. Not a sensor fault this
time and not a gap with readings hiding in it: `temperature`, `humidity` and
`rssi` stop together at 02:19:10Z and resume together at 17:45:56Z.

It was in perfect health right up to the last round — twelve rounds at 125 s
without a stumble, RSSI flat at −57/−58 dBm, temperature flat at 23.4 °C. No
drift, no retries, no warning. It went to sleep at 04:19:1x and nothing woke it
up, which is what losing the supply mid-sleep looks like from the outside.

**It is the node, not the house.** `bad` runs the same firmware, the same
`MainsDutyCycled` profile and the same 120 s sleep, and it sailed through that
minute — ten rounds between 04:16 and 04:33 — as did `schlafzimmer`,
`wohnzimmer` and `terrasse`. Over the whole day: `wohnzimmer` 8270 readings,
`schlafzimmer` 6420, `bad` 1647, `kueche` 339 and then silence. A broker, a
Wi-Fi or an archiver problem would not have picked one node out of five.

**So moving it to the socket strip's USB-C port did not fix it — it delayed it
by six hours.** The entry for 2026-09-13 explains why a USB-A charging port cuts
a sleeping node off, and that much was right; the conclusion drawn from it was
too narrow. A strip that switches itself off below a total standby draw takes
the C port with it, CC resistor or no CC resistor, and 04:19 is about when
everything else plugged into it would have gone quiet for the night. The node
did not come back on its own; it came back when somebody touched it.

**The supply has to be a plain wall adapter in a wall socket.** No strip with a
standby cut-off, no power bank, nothing that decides for itself whether anything
is still attached.

And next time it is silent, the first thing to look at is the red LED on the
board, before any query: it hangs on 3V3, the firmware never touches it, and
deep sleep does not dim it. **Dark means the supply went away** — this story
again. **Lit and silent means the board itself hung**, which is a different
investigation entirely and one nothing here has yet had to open.

## 2026-09-14 20:46 — kueche — deep sleep switched off

`kueche` stops sleeping between rounds. It still samples every 120 s and still
publishes the same three channels; what changes is that the board stays powered
and associated in between instead of cold-booting into each round.

**Why, and it is not about the node.** This one has now lost its supply twice on
the kitchen's socket strip — once on the USB-A port within minutes, once on the
USB-C port after six hours (see the two entries above). A board that draws tens
of microamps while asleep is below what such a port keeps itself alive for. A
board that never sleeps draws continuously, so the port has no reason to switch
off. That is a workaround for the supply, not a fix: with a plain wall adapter
this node could go straight back to duty-cycling, and should.

**Two consequences for the data.** The cadence moves from 125 s to 120 s — the
five seconds were the boot the node no longer does, and the timestamps tighten
accordingly from 18:46:25Z. And the board now warms the air its own SHT31 is
measuring again, which is the exact effect duty-cycling was introduced to remove
on 2026-09-13: **expect this node to read warmer from here, and do not read that
step as a warmer kitchen.**

How much warmer is worth measuring rather than assuming. The 24 hours before
this line sit at 23.4–23.5 °C duty-cycled, which is a clean baseline, and `bad`
runs the same box, the same firmware and the same profile as the control. The
2.5 °C measured in September was before this box had its chimney and its lid
grille; the honest guess is closer to the 0.9 °C measured on `schlafzimmer`, and
the data will say.

Done over the air, not by reflashing: `switch.kueche_deep_sleep` in Home
Assistant, which publishes a retained `smarthome/kueche/config/deep_sleep`. The
node reads it while publishing, writes it to its config sector, sleeps one last
time, and comes up awake on the next boot — so it takes two rounds, and the
first of them still looks unchanged. Verified in the flash blob (`magic BIRD`,
version 5, CRC ok, `deep_sleep = 0`) before it was visible anywhere else. It
survives power cycles, which is the point.


## 2026-09-17 10:39:41–21:49:28 — wohnzimmer, terrasse — two boards published under one name

The `terrasse` board was flashed with a `wohnzimmer` image and spent eleven
hours as a second living room. Both boards published to `smarthome/wohnzimmer/…`
the whole time, alternating roughly once a minute. The full account is in
[`commissioning.md`](commissioning.md#2026-09-17--flashed-with-a-wohnzimmer-image-and-eleven-hours-as-a-second-living-room);
this is what it does to the numbers.

**`terrasse` has a hole**, 10:39:41 to 21:51:12, in every channel it has. No
readings were lost to a fault — the node was publishing that whole time, just
under the wrong name. Anything attributed to the terrace in that window is
missing, not wrong.

**`wohnzimmer` is mixed, on four channels only.** `temperature`, `humidity`,
`rssi` and the two `reset_*` channels carry rows from both boards. The intruder
ran about 1.5 K cooler and 5 dB weaker — 23.7 °C at −52 dBm against the living
room's 25.2 °C at −47 — so the window shows as a series that alternates between
two levels rather than as a step. `readings_1m` averages the two together, which
is the form most charts will show it in.

**The other channels are clean.** `co2`, `scd41_temperature`, `scd41_humidity`,
`voc_index`, `nox_index` and all four `pm*` channels only ever came from the
real living-room board: the terrace board has none of those sensors, and a
sensor that does not answer is omitted rather than published as a zero. Anything
that needs a trustworthy wohnzimmer series across that window should use `co2`.

**The rows cannot be separated after the fact.** A reading carries a node and a
sensor, not a board, so nothing in QuestDB says which of the two wrote it. The
bimodality above is the only trace. Dedup is on `(timestamp, node, sensor)` and
the two boards stamped different instants, so no row overwrote another — the
window has roughly twice the expected row count, not half the values.

**Availability flapped for the whole window.** Both boards used the same MQTT
client id, so each connect evicted the other and fired its last will:
`smarthome/wohnzimmer/status` toggled `offline`/`online` about once a minute.
Home Assistant's wohnzimmer entities drop to *unavailable* across that window
in the recorder, and the archiver's status table records the same flapping.
None of that is a Wi-Fi or broker problem, which is what it will look like.

**Not affected:** `bad`, `kueche` and `schlafzimmer`, which published normally
throughout. And nothing before 10:39:41 or after 21:49:28 on either node.

## 2026-09-25 21:13 — terrasse — `visits` only becomes trustworthy the next day

`aaebba2` gave `Decision::Unexplained` a way out. Before it, a delta too large
for the drift band and too small for a visit froze the presence baseline
permanently, and the node counted nothing until someone tared it — it sat that
way for thirty-two hours from 2026-09-22.

So **`terrasse/visits` before 2026-09-26 understates reality badly**: 2–6 a day
through 2026-09-17…25 is a mostly blind node, against 53 and 32 on the two days
after the fix. Do not use the earlier figures as a baseline for "normal", and do
not read the jump as birds discovering the feeder.

The 94–165 a day of 2026-09-13…15 are not a usable comparison either, from the
other direction: that is a different counter state entirely, ending in the RTC
garbage of 2026-09-16 (reading 622, then reset to 3 — see the 2026-09-12 entry).

The two days that *are* trustworthy look like a bird feeder should: nothing
overnight, first counts around 08 h, a peak late morning, tapering through the
afternoon, and visit durations spread smoothly from 0.0 s to 22.9 s rather than
piled against the 1 s floor (`presence::MIN_COUNTED_VISIT_MILLIS`).

## 2026-09-26 onward — terrasse — `weight`: the zero walks down every hot afternoon

The terrace zero no longer comes back. Through each warm afternoon the published
weight ramps down by tens of grams, and in the evening — as the air cools back
through the temperature it started at — **it stays down**. Measured at matched
air temperature (18–20 °C), so the reversible thermal part cancels out:

| | zero at 18–20 °C | change |
|---|---|---|
| 2026-09-24 | +1.4 g | — |
| 2026-09-25 | −19.5 g | −20.9 g |
| 2026-09-26 | −26.0 g | −6.4 g |
| 2026-09-27 | −75.8 g | −49.9 g |

On 2026-09-27 it fell from −33 g at 10 h MESZ to a plateau of −97 g by 20 h,
the steepest hour being −20 g/h at 15 h.

**This is not the thermal correction failing.** `temp_coeff` is doing its job,
and the night of 26→27 proves it: the published weight sat flat at −32 ±1 g
while the air moved 12.3 → 16.6 °C and the reconstructed raw reading moved −54
→ −100 g. It removes something like 95 % of a 9.9 g/K mount. Nor is the
afternoon ramp thermal in the first place, because a temperature effect is
reversible by definition and this one is not: the air peaked at 28.7 °C at 15 h
and was back to 19.4 °C by 22 h while the weight went on falling. At 19.4 °C
against a 19.0 °C anchor the correction is +3.5 g, i.e. nothing — so the −97 g
is the **raw** zero, with no temperature left in it to subtract.

**It is the mount, and the likely mechanism is PLA creep.** `e873226` already
identified the printed clamps as the source of the 9.9 g/K; what is new is that
they now yield *permanently* when warm. The load path runs through a cantilever
— the hanger's spine — and the entry for `terrasse_beam_hanger` in the README
predicts this exact failure: "what bends does not spring back exactly, and that
shows up as hysteresis in the weight." PLA under sustained load softens far
below its glass transition, and a dark printed part in direct sun runs well
above the shaded air the SHT31 reads. That the daily step is growing
(−6 g, then −50 g) is what creep does, and it is why this will not settle on its
own.

**What it does to the data.** Absolute grams from 2026-09-25 onward are not
comparable between days, and within a hot afternoon not comparable between
hours. What survives is *differences over seconds* — the weight of a bird on the
cell, since `presence` works against a baseline that tracks — so `visit` and the
arrival logic are sound even where the absolute number is meaningless.

**`visits` is not affected.** Worth stating because the counter jumps from 2–6 a
day to 53 and 32 exactly here, which looks like the drift manufacturing arrivals.
It is not: the counts are *anti*-correlated with the ramp — zero counted in every
hour past 15 h on 2026-09-27, which is where the drift is steepest — and a
monotonically falling reading cannot produce an `Arrived` at all, because
`delta = raw − baseline` stays negative while the baseline follows it down. The
jump is the `Unexplained` freeze being fixed on 2026-09-25 21:13 (`aaebba2`); see the
note on the visit counter's usable range above.

**Re-taring is not the fix.** It resets the offset and the zero walks again by
the next afternoon; recalibrating `temp_coeff` chases a coefficient that is
itself moving while the spine creeps. The fix is mechanical.

**What was done about it, 2026-09-27.** `RAIL_T` went from 14 to 20 mm in
`models.py` — 2.9× the stiffness, about half the bending stress, and it grows
downward into open air so the 4 mm clearance under the bar is untouched. The
hanger goes from 19.9 to 27.6 cm³ and its counterbore from 10 to 16 mm deep; the
bolt stays M5 × 16. `terrasse_beam_hanger.stl`/`.step` re-exported.

**The reprint has not happened yet, so nothing below this line has changed in the
data.** And the geometry is the smaller half: that spine was **PLA**, and the
reprint wants PETG at the least, ASA for preference. Expect a step in `weight`
when the part is swapped — it is a new mount, so it needs `scale_factor`
re-checked in the fixture, then `temp_coeff` re-measured over a night, then a
tare, in that order. Add an entry here when it happens.

## 2026-09-17 onward — bad — stops every few days, and it is not the power

Four outages in a fortnight, all of them this node alone:

| stopped | resumed | down | had run |
| --- | --- | --- | --- |
| 17.09 08:03:28Z | 08:45:32Z | 0.7 h | 63 h |
| 20.09 17:02:36Z | 18:05:22Z | 1.0 h | 80 h |
| 25.09 17:41:52Z | 20:17:46Z | 2.6 h | 120 h |
| 28.09 19:39:25Z | 29.09, by hand | 21 h+ | 71 h |

**Read the gaps as missing, not as flat.** Everything is absent — temperature,
humidity, `rssi` — because the node is absent, and a chart drawn across them
will interpolate a line through hours that were never measured.

It stops mid-stride. The last rounds before each outage are indistinguishable
from any other: the 120 s cadence holds to the final publish, RSSI sits at
−59 dBm, the readings are flat. Afterwards the broker sees **nothing at all** —
not a failed publish, not a connection attempt.

**It is the node, not the house.** Through every one of those windows the other
four published normally. Over the day of the last outage: `wohnzimmer` 1334
rounds, `schlafzimmer` 1425, `kueche` 717, `bad` 83 and then silence.

What has been ruled out, and by what:

| | |
| --- | --- |
| broker, Wi-Fi, archiver | the other nodes publish straight through |
| a watchdog reboot | reason `0x07` never reported, before or after |
| a brownout | reason `0x0F` never reported |
| loss of power at the board | **the red 3V3 LED is lit while it is dead** |
| a mis-computed sleep | `publish_interval` is a constant 120 s for a node with no load cell; the seasonal night cadence never touches this path |
| the HX711 pad hold | `park_scale` returns immediately when no scale is enabled |
| condensation, showers | humidity moved 0.3 points in the six hours before the last one |

Two possibilities survive, and **the archive cannot separate them**: the board
is asleep and the RTC timer never fires, or it is awake and hung somewhere the
watchdog does not reach. Deep sleep switches the watchdogs off, which is why the
first would leave exactly this trace — powered, silent, no reset ever reported.

**Why the existing counters could not answer it**, which is the lesson worth
keeping: `reset_count` lives in RTC RAM, and an outage long enough to matter
clears it, so `latch` starts again at 1 with the reason set to `POWER_ON` — the
same pair a node reports when it has simply been running quietly for days. Both
cases read `reason 1, count 1`. From 2026-09-29 a second counter sits in flash
at `0xC000+0x100` and survives power, so the next time this happens the question
answers itself: if `boot_count` has climbed, the board restarted; if it has not,
it never did, and whatever went wrong happened with the power still on.

Next step is a measurement, not a guess: with deep sleep switched off this node
either stops failing — in which case it was the sleep it did not come back from
— or fails while awake, where the watchdog can reach it and the reset reason
will finally have a name.

## 2026-09-29 19:25 — bad — updated over the air, and `boot_count` starts here

`bad-38caeb5` went on over the air: the round at 17:25:08Z took the offer, and
the node was publishing again at 17:25:25Z. Seventeen seconds to fetch 778 KB,
write the slot it was not running from, restart into it and report. Four rounds
at 117 s since.

**`boot_count` exists from this timestamp and not before**, so an empty series
before it is the channel not existing, not a node that never booted. It starts
at 1 because the flash sector was blank — the number only becomes interesting at
the *next* unexplained stop.

One thing the update itself demonstrated, and it is worth knowing when reading
the other counter: `reset_count` came back as 1 across the OTA, not 2, although
the restart was a software reset the node did to itself. The RTC-RAM statics
almost certainly land at different addresses in a different binary, so the epoch
tag no longer matched and `latch` took its "not ours" branch. **Expect
`reset_count` to restart at 1 after every over-the-air update** — which is one
more reason the counter that answers "did it restart at all" had to live in
flash.

## 2026-09-29 — the whole fleet — why a hung node never came back

Read alongside the `bad` entry above, which this explains, and the two
`terrasse` silences in [`commissioning.md`](commissioning.md), which it explains
too.

Two things were true of every node in the house, and each was enough on its own:

**No watchdog was armed.** `esp_hal::Config::default()` sets every one of
`swd`, `rwdt`, `timg0` and `timg1` to `Disabled`, and `main` only ever set the
CPU clock. Nothing on the chip could restart a stuck board.

**A panic stopped rather than restarted.** `esp-backtrace` 0.14.2's panic
handler and exception handler both end in `halt()` — `loop { continue; }`.

So a panic or a CPU exception left the board spinning with its power LED lit,
off the air, until somebody removed power. Which is exactly what was seen, and
why `reset_reason` never had anything to report: **nothing had reset**. The
counter was working; there was nothing to count. It also explains why the same
failure appeared on `bad`, on mains, and on `terrasse`, on a battery — the one
thing they share is the firmware.

**From this firmware on**, a panic restarts the node and says so: `reset_reason`
publishes `0x100`, which is not a hardware code, and `boot_count` in flash
counts it. A hang that is not a panic is restarted by the TIMG0 watchdog after
60 s and reports `0x07`. **A silence with nothing reported afterwards therefore
means something new from here** — the two explanations it used to hide are now
both loud.

The watchdog is TIMG0 and deliberately not the RWDT: the RWDT lives in the RTC
domain, which stays powered through deep sleep, so arming it would reboot a
sleeping node mid-sleep — most of a duty-cycled board's life. TIMG is in the
digital domain, which deep sleep switches off, so it covers the awake window and
nothing else.

**What this does not do is fix whatever panics.** It converts an invisible
permanent death into a visible restart. If a node starts reporting `0x100`
regularly, that is the bug arriving in the archive where it can be read — which
is the first time it will have been.

## 2026-09-29 22:08 / 22:16 — bad, terrasse — the watchdog build, and what `boot_count` starts at

`…-29a4d62` — the firmware that restarts a panicked node instead of leaving it
dead, from the entry above — went on over the air:

| | took it | `boot_count` after |
| --- | --- | --- |
| `bad` | 22:08 | **2** |
| `terrasse` | 22:16 | **1** |

**Neither number is a count of failures**, and that is the only thing here worth
remembering. `boot_count` starts at 1 on the first boot of the first firmware
that has it, because the flash record was blank until then. `bad` reads 2
because it had already been updated once that evening — the 19:25 install of
`38caeb5`, then this one. Read the *increments* from here, not the values.

`reset_count` behaved differently on the two, which is worth knowing before it
is read as a fault: `bad` came back with 1 and `terrasse` with 2, across the
same kind of software reset. RTC RAM survives a restart only while the statics
land at the same addresses, and that depends on the two images, not on the node.
**`reset_count` is only meaningful between updates**; `boot_count` is the one
that spans them.

The two were picked first on purpose. They are the nodes the fix is *for* —
`bad` has stopped four times in a fortnight and `terrasse` twice, and on the
terrace a power cycle means a ladder. `wohnzimmer`, `schlafzimmer` and `kueche`
carry the same defect and have not yet been bitten by it, so they wait for these
two to run a night.

## 2026-10-06 18:59 / 19:05 — kueche — an unrecorded flash, and deep sleep back on

Two changes within six minutes of each other, neither written down at the time.
This entry exists because the gap cost an evening on 2026-10-07: the archive
showed a node misbehaving from 18:59 onwards and there was nothing anywhere
saying what had been done to it.

- **18:59:12** — firmware went on over the air. `boot_count` appears for the
  first time on this node and `reset_reason` reads 3, the software reset an
  update performs on itself.
- **19:05:05** — `switch.kuche_deep_sleep` went `off` → `on`, undoing the
  2026-09-14 workaround above.

**Deep sleep is really in effect**, and the two numbers that prove it are worth
keeping because neither is the one you would reach for first. The cadence moved
from 120.6 s to 125.2 s — those five seconds are the boot the node does again —
and the temperature fell from a 23.00 °C median to 22.70 °C, because the board
stopped warming its own SHT31. That is the 2026-09-14 entry's prediction running
backwards, and it is the cheapest available proof that a node is duty-cycling.

**What it cost, measured over the 22 hours to 17:07 on 10-07:** 635 rounds, 47
of them with no publish at all (7.4 %), and four watchdog resets. Before the
switch: nine days, zero gaps.

**Every gap is one whole round, and the arithmetic says so.** 42 of them are
266 s against a computed 20 s `WIFI_BUDGET` + 120 s sleep + 5 s awake + 120 s
sleep = 265 s, and five are 406 s against two failed rounds at 405 s. The
distribution is bimodal — the good rounds sit at 125.2 s within 0.1 s — so this
is not a link that is sometimes slow. The join is refused outright, and the flat
five-second retry then spends the rest of the budget.

**And the refusal clears by itself.** 42 of the 47 lost rounds were followed by
an entirely normal one two minutes later. That is what `3fe9c6b` is for.

**`bad` is not innocent, it is quieter.** Same `MainsDutyCycled` profile, same
120 s, same firmware in the shared path, and it also carries `reset_reason` 7
and a `boot_count` of 11 — but 0 to 2 gaps a day against `kueche`'s 47. RSSI is
−62 against −59 dBm and flat on both. **Why one node's joins are refused and the
other's are not is still unanswered**, and it is not in this repository.

## 2026-10-07 21:19 / 21:21 — kueche, bad — the join-backoff build

`…-6c81631` went on over the air, `kueche` first and `bad` two minutes later,
both confirmed by a full round within one cadence:

| | took it | `boot_count` after | `reset_count` after |
| --- | --- | --- | --- |
| `kueche` | 21:19:43 | 6 | 6 |
| `bad` | 21:21:11 | 12 | 4 |

It carries two changes. `3fe9c6b` retries a refused join at 500 ms doubling to
30 s instead of a flat five seconds, paced by a task-local counter so each round
keeps its fast first retry. `6c81631` doubles the *sleep* after a round that
never reached the broker — one doubling on mains, four on a cell — and resets it
on the first round that lands.

**What to read in a day**, and the honest version of each:

- **Gaps per day on `kueche`.** 47 in 22 hours is the number to beat. Anything
  above zero still means joins are being refused; the fix only stops a refusal
  from costing the whole round.
- **`reset_count` on both.** The four watchdog resets are **not** addressed by
  either commit. An expired `with_timeout` is a clean abort, not a stopped
  executor, so if 7s keep arriving the hang is a second bug and still open.
- **`bad` as a control is gone.** It was updated in the same minutes, which was
  asked for and is worth knowing when reading the next week: there is no longer
  a node on the old firmware to compare against.

`reset_count` reads differently on the two afterwards — 6 against 4 — and that
is the 2026-09-29 note repeating itself rather than a fault: RTC RAM survives a
restart only while the statics land at the same addresses, so the count is only
meaningful between updates. `boot_count` is the one that spans them.

## 2026-10-07 22:33 — solarleuchte — no temperature because the image predates it

The archive has `battery_*`, `rssi` and the reset counters from `solarleuchte`
but no temperature or humidity, although `src/node.rs` has `sht31: Slot::on()`
for it (#40). Read off the broker: the board publishes
`solarleuchte-e3c08a2` on `ota/version` and announces no temperature entity at
all. `e3c08a2` is older than `d82fd60` (2026-10-06), the commit that switched
the slot on, so **the SHT31 has never been flashed to this board**. There is no
evidence yet on whether the sensor itself is fitted or wired correctly; the
next flash will answer that.

## 2026-10-08 04:11 / 04:13 — bad — first image on esp-hal 1.1

`bad-0.2.0-42dd3b7`, the esp-hal 1.1 port (#32), offered over the air at
04:11:54 and running at 04:13:24: slot 1 → **slot 0**, `seq` 6 → **7**. It is
the first image built with espflash 4.x (`save-image`) and the first carrying an
ESP-IDF app descriptor; the board's bootloader, flashed by espflash 3 on
2026-09-17, took it without complaint. Header bytes match the old images
(chip id 5, min rev v0.3).

| | before | after |
| --- | --- | --- |
| version | `bad-6c81631` | `bad-0.2.0-42dd3b7` |
| `boot_count` | 12 | 13 (the update's own reset) |
| `reset_count` | 4 | **1** |
| round spacing | ~125 s | 123–132 s |

**`reset_count` dropping to 1 is the RTC RAM re-layout, not an event.** The
statics moved with the HAL, so the counter restarted; `boot_count`, which lives
in flash, carried on. Read `reset_count` across this line as two series.

Seen working on the first four rounds: deep-sleep wake (no new boots), Wi-Fi
join, NTP timestamps, RSSI through `esp-wifi-sys-esp32c3`, the SHT31-D, the
version topic in the new `<node>-<release>-<commit>` form (#39), and no
rollback — still slot 0 after the third attempt, so the trial was confirmed.
The other five nodes are still on their old images.

## 2026-10-08 10:21 / 10:22 — kueche — esp-hal 1.1

`kueche-0.2.0-58aaadd`, the same port as `bad` six hours earlier (only the fleet
log differs between the two commits), offered at 10:21:31 and running at
10:22:38: slot 1 → **slot 0**, `seq` 4 → **5**.

| | before | after |
| --- | --- | --- |
| version | `kueche-6c81631` | `kueche-0.2.0-58aaadd` |
| `boot_count` | 7 | 8 (the update's own reset) |
| `reset_count` | 7 | **1** (RTC RAM re-layout, as on `bad`) |

Three rounds by 10:26 at 133 s and 123 s apart, no new boots, still slot 0 after
the third attempt, so confirmed. `bad` had by then run six hours on the port
with `boot_count` unchanged at 13.

## 2026-10-08 10:49 / 10:51 — schlafzimmer — esp-hal 1.1, and a `reset_reason` 256 that is not a panic

`schlafzimmer-0.2.0-6f2c56e` offered at 10:49:30, picked up at the end of the
10:50:29 round, running at 10:51:00: slot 1 → **slot 0**, `seq` 2 → **3**. The
node came from `c0b36ee` (2026-09-18), older than `boot_count`, so that counter
starts here at 1.

**`reset_reason` reads 256 (`PANIC`), and the evidence says it is RTC RAM, not a
crash.** The new image's `FLAGS` word sits where the old image left other data,
and bit 4 happened to be set. A panic in the new image would have meant a second
boot, and `boot_count` stayed at 1 across the next three rounds. What it cannot
rule out is a panic *before* `note_boot` on the very first start — the same
init path ran clean on `bad` and `kueche`, which makes that unlikely. The value
is latched and will read 256 until the next reset; it is not repeating.

CO₂ was missing from the first round only — periodic mode had not produced its
first sample yet — and present from 10:52 (569, 592 ppm), which is the SCD41's
first run on the 1.1 port. Rounds at 60 s, still slot 0 after the third, so
confirmed.

## 2026-10-08 11:15 / 11:16 — wohnzimmer — esp-hal 1.1

`wohnzimmer-0.2.0-22b9205` offered at 11:15:37, picked up at the end of the
11:15:53 round, running at 11:16:25: slot 1 → **slot 0**, `seq` 4 → **5**.
Like `schlafzimmer` it came from `c0b36ee`, so `boot_count` starts at 1 here.

**`reset_reason` 256 again, and that makes the RTC RAM reading stronger.** Both
nodes that came from `c0b36ee` show it on their first boot with `boot_count` 1
and never again; neither node that came from `6c81631` does. That is what a
layout artefact looks like — the old image's data at the new `FLAGS` address,
the same data on both boards — and not what a crash looks like.

This is the first run of the SDS011 and the SGP41 on the 1.1 port, so the UART
and the shared I²C bus with three devices on it:

- **SDS011:** PM2.5 0.6 µg/m³ in the very first round.
- **SCD41:** absent from the first round, as on `schlafzimmer`, then 660–664 ppm.
- **SGP41:** `voc_index` read 3 at 11:17 and 87 at 11:18 against 80–81 before
  the update. The gas-index algorithm restarts its learning on every boot; the
  dip is that, not the port.

Rounds at ~63 s, still slot 0 after the third, so confirmed. Four of six nodes
are on the port; `terrasse` and `solarleuchte` remain.

## 2026-10-08 11:24 / 11:46 — terrasse — esp-hal 1.1, and `visits` back to 0

`terrasse-0.2.0-adee5d9` offered at 11:24:39. The node is on its cell and only
reaches the broker on its heartbeat, so it found the offer at the end of the
11:35:13 round, wrote the slot and restarted, and reported from the new image on
the next heartbeat at 11:46:46: slot 1 → **slot 0**, `seq` 6 → **7**. That
first publish is the confirmation. Heartbeat spacing 11:23:07 → 11:35:13 →
11:46:46, about 12 minutes, unchanged by the update.

| | before | after |
| --- | --- | --- |
| version | `terrasse-29a4d62` | `terrasse-0.2.0-adee5d9` |
| `boot_count` | 1 | 2 (the update's own reset) |
| `reset_count` | 2 | 1 (RTC RAM re-layout) |
| `visits` | 202 | **0** |
| weight | −60.4 / −61.4 | −60.0 |
| cell | 4.00 V, 77 % | 4.00 V, 78 % |

**`visits` dropping to 0 is the same re-layout, not lost birds.** The counter
lives in RTC RAM behind a checked pair (`scale::VISITS_MAGIC`); at its new
address the pair did not check out, so it started again from zero, as designed.
It is a `total_increasing` entity, so Home Assistant books the drop as a meter
reset and its statistics carry on. Anything summing the raw series across this
line has to treat it as two.

First run of the HX711 and the battery path on the 1.1 port. The weight reads
where it read before, which says `release_scale_pad` let go of the pad and the
driver clocks normally. Whether `park_scale` still holds the pad through deep
sleep — the ~4.5 mA it saves — is not visible in one round; the cell voltage over
the next days is the evidence, against the drain before the update. SHT31-D 14.2
°C / 92 %, RSSI −71. `reset_reason` read 3, not the 256 the two nodes from
`c0b36ee` showed.

Five of six nodes on the port; `solarleuchte` remains.
