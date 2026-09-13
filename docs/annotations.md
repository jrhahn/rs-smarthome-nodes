# What happened to the fleet, and when

A dated list of everything that changed what the numbers *mean* — a sensor
replaced, a counter reset, a cadence changed, a gap and its cause. The readings
themselves are in QuestDB; this is the part that cannot be derived from them.

Why bother: in two years a step in a series is indistinguishable from a real
change in the house. The terrace's weight jumped on 2026-09-09 because the scale
was recalibrated, not because a heavier bird arrived, and nothing in the data
says so. [`long-term-history.md`](long-term-history.md) argues the case at
length; this file is the cheap half of it, kept by hand until the `annotations`
table exists.

**One line per event.** Date, node, channel if it matters, what happened, and
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

