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
