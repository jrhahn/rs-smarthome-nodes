# Home Assistant

No YAML. Every node announces itself over retained MQTT discovery, both
directions:

- the **readings** become `sensor` entities (weight, temperature, humidity, CO₂,
  PM2.5/PM10, VOC/NOx index, battery, RSSI);
- the **calibration and tuning knobs** become `number` / `switch` / `button` /
  `light` entities, filed under the device's *Configuration* section.

Adding a node to the fleet therefore needs no change on the Home Assistant side
at all. The only requirement is the MQTT integration with discovery enabled (the
default) on the `homeassistant/` prefix.

## Topics

Each node **announces itself**: whenever what it would announce has changed (see
[below](#forcing-a-re-announce)) it publishes one retained config message per reading to
`homeassistant/sensor/<node>/<key>/config`, so Home Assistant creates the device
and its entities without any YAML. Values are ready to use — grams, °C, %, ppm,
µg/m³ — with no template maths.

| Node | State topics |
| --- | --- |
| `terrasse` | `smarthome/terrasse/weight`, `/temperature`, `/humidity` (SHT31-D), `/battery_voltage` — each appearing as its slot is switched on |
| `schlafzimmer` | `smarthome/schlafzimmer/co2`, `/temperature`, `/humidity` (SHT31-D), `/scd41_temperature`, `/scd41_humidity` |
| `wohnzimmer` | the same five, plus `/pm25`, `/pm10` (humidity-corrected), `/pm25_raw`, `/pm10_raw` and `/voc_index`, `/nox_index` |
| `kueche` / `bad` | `smarthome/<node>/temperature`, `/humidity` |
| `solarleuchte` | `smarthome/solarleuchte/temperature`, `/humidity`, `/battery_voltage`, `/battery_percent`, and the lamp as a `light` |

Every node also publishes `rssi` and the reset diagnostics `reset_reason`,
`reset_count` and `boot_count`; the scale adds `visits` and `visit`, and every
battery node `battery_percent` beside `battery_voltage`.

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
all.

## Configure & calibrate from Home Assistant

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
| `reset_visits` (button) | zero the visit counter, which lives in RTC RAM and survives reflashes and short power cuts | with load cell |
| `tare` (button)     | re-zero: takes a burst of fresh readings on the empty feeder and adopts their median as `offset` *and* as the presence baseline, and records the air temperature as the correction's anchor | with load cell |
| `temp_coeff`        | grams the zero moves per kelvin, signed; `0` disables the correction (default) | with load cell **and** SHT31 |
| `idle_interval`     | deep-sleep seconds while empty | battery |
| `active_interval`   | deep-sleep seconds for a load that outlasted its awake visit window (snow, a twig) — a normal visit is watched awake and never uses this | battery |
| `heartbeat_interval`| seconds between periodic temp + weight publishes with no visitor (default 600) | battery |
| `night_interval`    | deep-sleep seconds between polls while it is dark, `0` to poll at `idle_interval` around the clock (default `0`) | battery |
| `night_margin`      | minutes of daylight held clear at each end of the dark window (default 30) | battery |
| `deep_sleep` (switch)  | `0` = stay awake with Wi-Fi up (bench testing on USB), `1` = sleep between rounds as the profile intends | any sleeping node |
| `scd41_temp_offset` | °C the SCD41 subtracts for its own self-heating (default 4.0) | with SCD41 |
| `sds011_kappa`      | κ for the PM humidity correction, `0` disables it (default 0.25) | with a compensated SDS011 |
| `enabled` (light), `brightness` | the lamp, on/off and a factor on the compiled duty ceiling | `solarleuchte` |
| `reannounce` (button) | publish discovery again on the next connect | every node |

Only the knobs that do something on a given node are announced. The
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
[`commissioning.md`](commissioning.md).

On a blank flash the firmware falls back to built-in defaults
([`src/config.rs`](../src/config.rs) — `offset` mid-scale, `scale_factor` 420, `threshold` 10 g,
2 s / 10 s idle/active intervals, 600 s heartbeat).

**The night cadence (`night_interval`):**

Birds do not feed in the dark, so every wake-up between dusk and dawn is the
node confirming that nothing is happening. On `terrasse` that is some 5 400 of
them a night, and the measured cost of a wake-up — boot, four HX711
conversions, the sensor reads — is **1.0 s awake out of a 6.0 s cycle**. Setting
`night_interval` to 300 s collapses those 5 400 into 72 and saves about **27 mAh
a day, a fifth of the node's whole budget**, without losing a single visitor.

The window is not configured. It is sunset to sunrise for the day, computed on
the node from [`src/solar.rs`](../src/solar.rs) — twenty-four points off the year's
curve for this fixed location and a straight line between them, worst error two
minutes. So it follows the season with nothing to edit twice a year, and there
is no local-time-versus-UTC trap to fall into.

`night_margin` is the one knob over it: how far inside the dark the window sits.
The margin applies outward at both ends, because the two errors do not cost the
same — opening too early loses visits that can never be recovered, closing too
late costs a few minutes of polling.

It needs the node to know roughly what time it is, which it learns from the NTP
sync on every publish round and carries across deep sleep in RTC RAM (see
[`state::clock_ms`](../src/state.rs)). That clock is deliberately coarse and is
never used for timestamps — `clock.rs` explains why a drifting clock makes a
worse timestamp than none at all, and that argument is untouched. A node that
has not synced since it last lost power simply polls at the day cadence.

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
   on it, then divide the published grams by the kelvin between them, and enter
   that number — sign and all — in the *Temperaturgang* number.

   The sign is the grams the *reading* rises per kelvin of **warming**, because
   that is what the correction subtracts. The terrace zero goes *down* as it
   cools, so it goes up as it warms, so its coefficient is **positive**: about
   `+9.9`. Which is exactly what dividing published grams by kelvin gives you —
   enter the measurement as measured and the sign takes care of itself. A mount
   that pulls the other way gets a negative one, which is why the field is
   signed at all.

   Getting it backwards does not merely fail to correct: `corrected = raw −
   (air − tare_temp) × temp_coeff` with the sign inverted *doubles* the drift,
   to some 20 g/K on this mount. The check is a night — a correct coefficient
   leaves the published weight flat while the air moves several kelvin.
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

## Example dashboard card

Add to a dashboard (Raw configuration editor). Entity ids follow from the node
name Home Assistant assigns the device — adjust if yours differ. The `title:`
lines are just card headings, not entities: if one turns up in a dashboard and
is not wanted, it is this YAML that put it there, not the firmware.

```yaml
type: vertical-stack
cards:
  - type: entities
    title: Vogelwaage
    entities:
      - entity: sensor.terrasse_gewicht
      - entity: sensor.terrasse_temperatur
      - entity: sensor.terrasse_luft_temperatur
      - entity: sensor.terrasse_luft_luftfeuchtigkeit
  - type: entities
    title: Kalibrierung
    entities:
      - entity: button.terrasse_tarieren
      - entity: number.terrasse_kalibrierfaktor
      - entity: number.terrasse_tara_offset
      - entity: number.terrasse_ausloseschwelle
  - type: history-graph
    hours_to_show: 24
    entities:
      - entity: sensor.terrasse_gewicht
```

## Forcing a re-announce

You mostly should not have to. Discovery re-announces whenever **what it would
say changes**: the firmware hashes every topic and payload it is about to
publish and keeps that digest in RTC RAM (`discovery::announcement_tag`). Switch
a sensor on in `src/node.rs` and reflash, change a cadence, move a board to a
different node — the next connect notices and re-announces. Nothing changed
means nothing is sent, so a battery node spends no airtime on it.

That replaced a plain "have I announced yet" bit, which could only ever answer
*yes* — the one answer that cannot be checked. It was wrong for six days on the
living-room node: the bit was set while its SDS011 slot was still off, and after
the slot was switched on the four PM entities published to the broker with
nothing in Home Assistant subscribed to them. A reflash did not fix it, a reset
did not fix it, and neither did pulling the USB cable for several seconds — the
board's rails decay slower than the RTC domain forgets, so the bit came back
still set.

If you do want to force it, clear the digest by deleting the retained configs
and power-cycling, or simply change any tuning knob and change it back:

```bash
mosquitto_pub -h <broker-ip> -t 'homeassistant/sensor/<node>/<key>/config' -r -n
mosquitto_pub -h <broker-ip> -t 'homeassistant/number/<node>/<key>/config' -r -n
```
