# Commissioning log

What is physically built, on which board, and what still has to happen to it.

The rest of `docs/` describes how the firmware is *meant* to work. This page
records the state of the actual hardware, which nothing in the code can tell
you: a node's slot being `Slot::on()` says a sensor is expected, not that one
is soldered on.

Keep it honest rather than complete. A stale entry here is worse than a missing
one, because it will be believed.

---

## Boards

Identified by the MAC in the USB serial descriptor, which is also what
`espflash board-info` reports. **Check it before flashing.** Two boards were
plugged in at once during the 2026-09-04 session and the wrong one got written;
the MAC is the only thing that distinguishes them, and reading it takes a
second.

| MAC | Node | Power | Address |
| --- | --- | --- | --- |
| `e0:72:a1:18:e2:c0` | `terrasse` | battery | 192.168.1.81 |
| `ac:27:6e:80:51:f8` | `wohnzimmer` | mains | — |
| `ac:27:6e:82:43:94` | `kueche` | mains, duty-cycled | 192.168.1.30 |
| `ac:27:6e:7e:10:a0` | `schlafzimmer` | mains | — |
| `ac:27:6e:7f:a6:b4` | `bad` | mains, duty-cycled | — |

Complete as of 2026-09-10: the last two were read off `espflash board-info`
during the RSSI rollout, so every board in the fleet is now identifiable
without opening anything.

---

## `terrasse` — outdoor, battery

Replaced the node formerly called `draussen`. It carries the bird-feeder scale.

**Verified on hardware 2026-09-04:**

- **HX711** on `D0`/`D1`. Resting readings scatter about ±70 counts out of a
  ±8.4M range — a clean, low-noise chain. A press produced a real presence
  edge: `visit` is published only on the arrival path, so seeing it proves the
  threshold crossing fired and `watch_visit` ran through the press.
- **SHT31-D** at `0x44`, no prefix — it owns the plain `temperature` and
  `humidity` keys.
- **Battery divider** on `D2`, reading 4.11 V off the XIAO's charger with no
  cell fitted. That is the check worth doing before connecting a LiPo: it
  proves the divider is wired and scaled while nothing is at stake.

**Verified on hardware 2026-09-06:**

- **The cell is connected and the node runs on it.** With USB unplugged it
  keeps publishing, `battery_voltage` included, which is the only proof that
  matters — the divider sits on the rail the charger drives, so a reading near
  4.1 V with USB attached is indistinguishable from the 4.11 V measured above
  with no cell at all.
- **It charges.** 3.80 V before a short USB session, 3.93 V after.

**Verified on hardware 2026-09-10 — the scale is calibrated:**

- **Zero taken in the working position**, hanging outdoors with the bird house
  fitted and empty. The mount's own load, the cord tension and the beam's
  orientation all enter the raw value, so a zero taken on the desk would carry
  that whole difference as a standing error. Residual reading afterwards:
  0.5–2.1 g, about a thousandth of the calibration mass.
- **`scale_factor` = 1862.8 ticks/g**, from 545 g of known mass reading 2417.2
  with the placeholder 420 still in place. Verified by the same mass reading
  **545.2 g** afterwards — 0.04 % out.
- **The bird house and mount weigh 451 g**, which is what the +1999 g jump on
  hanging it works out to once the factor is right. Worth writing down as the
  sanity check it is: an implausible number here means the factor is wrong.
- **The threshold was too sensitive, not too coarse.** With the placeholder
  factor, `threshold_grams: 10.0` amounted to `10 × 420 / 1862.8` ≈ **2.3 g** of
  real mass. That, not a mechanical fault, is what had the node publishing
  visits all day against an uncalibrated scale.

### Taring takes its own readings

Since 2026-09-13 a tare **measures**: the press is noticed while the node is
draining retained config, and once that round is over — back where the load cell
is reachable — it takes `presence::TARE_SAMPLES` readings, about 1.6 s at the
cell's ~10 SPS, and adopts their **median** as both the gram zero and the
presence baseline. If the readings spread more than `TARE_MAX_SPREAD` ticks it
refuses, says so, and leaves the old zero standing; press it again once things
are still.

The median is there because the press arrives retained and is acted on whenever
the node next wakes, which nothing stops a bird from coinciding with. A landing,
a hop or a gust occupies a minority of the window and the median ignores it. A
bird that sits *still* through the whole 1.6 s is not caught and cannot be — no
measurement taken at tare time can tell that weight from the feeder's own. That
one is yours to avoid, which is why the window is short enough to stay inside
"I am looking at it".

**What this replaced, and why it is worth knowing.** `Config::apply` used to set
`offset = tare_ref`, and `main` passed `state::baseline()` — the drift-tracked
presence baseline, not a reading taken when the button was pressed. That made
taring inherit every problem the baseline had:

- *You had to wait for the baseline to catch up.* Hanging the house is a load
  far above the threshold, so the node read `Arrived`/`Staying` and only
  absorbed it once `STUCK_AFTER_SECS` (600 s) had passed. Taring before that
  copied a stale zero.
- *Removing a calibration mass left the baseline stuck.* Once absorbed, taking
  the mass off is a large negative delta, which `decide` classifies as
  `Unexplained` — deliberately leaving the baseline alone, with nothing to ever
  move it back. Taring then copied the mass-inclusive value and showed roughly
  minus its weight. The same trap caught any remount that did not reproduce the
  beam's preload.
- The documented way out was to raise `threshold` until the step fell inside the
  drift band (`threshold/4`) — 3000 g for a 545 g mass — wait for the `Quiet`
  drift to pull the baseline back, then set it to 10 g again. The arithmetic
  still holds if you ever need it: the drift keeps 15/16 of the error per 2 s
  round, so 600 s is 300 rounds and `(15/16)^300 = 3.9e-09`, leaving 0.004 ticks
  of 1 015 226. You should not need it. A tare now writes the baseline itself,
  which is the same escape in one button press.

The firmware accepts any finite `threshold ≥ 0` over MQTT; the 500 g maximum is
only the Home Assistant slider's.

### The socket is wired with the colours crossed, and that is correct

On this pigtail, **black goes to `B+` and red to `B−`.**

That looks wrong and is not. The JST-PH housing is keyed, so the cell's plug
mates one way only; what is *not* standardised is which contact of the housing
carries positive. This pigtail was made with the opposite convention from the
cell's plug, so matching the colours at the solder joints would have mated them
backwards. **Decide this by continuity and a voltmeter, never by wire colour** —
probe which socket contact reaches `B+`, then check the cell's plug puts red on
that contact.

### A reversed cell reads exactly like a broken divider

It was mated backwards the first time. Nothing was damaged and the cell stayed
cold, but the failure was thoroughly misleading and cost an evening:

1. The 1S protection board **latched off**. `P+`/`P−` collapsed to 1.2 V while
   the cell's own terminals still held 3.8 V.
2. The divider sits on the *protected* rail by design (see the module note in
   [`src/battery.rs`](../src/battery.rs)), so it measured that dead rail and
   reported **exactly 0 mV** — tripping the firmware's "no cell at all — check
   the divider is fitted" warning.
3. The board went on running the whole time, because it was on USB.

So the symptom was a firmware message naming the divider, on a node that was
otherwise healthy, with an intact divider. **0 mV does not distinguish "divider
open" from "protected rail dead."** The signature that separates them is
measuring the two rails against each other: **3.8 V at the cell, 1.2 V at
`P+`** can only be the protection board.

Recovery needed no soldering: plugging in USB applies the charger's voltage
across `P+`, which is the ordinary way to unlatch a DW01A-class board. It went
1.2 V → 4.1 V and has behaved since. The protection did its job — it shut off
instead of dying.

**Not done:**

- **Not calibrated.** `offset` is the factory default, so weight publishes as
  roughly -20 kg. Calibration is `tare` on an empty, mounted pan and then
  `scale_factor` against a known mass, both over the Home Assistant knobs, no
  reflash. It waits on the enclosure.

**Answered 2026-09-09 — the runtime config path was broken, and is fixed:**

`deep_sleep = 0` sat retained on the broker for days and never took effect.
The cause was two `subscribe_to_topic` calls in a row in `publish_samples`.
`rust-mqtt` polls for its own SUBACK and discards anything else that arrives —
*"If an application message comes at this moment, it is lost"*, says
`client.rs:145`, and then it returns an error. So the second subscribe swallowed
the retained config the first one had just asked for. Deterministic, whenever
retained config existed.

It now sends one SUBSCRIBE carrying both filters. Verified on hardware:
`config: heartbeat_interval = 660` followed by `config updated and saved to
flash`. That matters here beyond the flag, because **`tare` and `scale_factor`
go through the same code** — the calibration above could not have landed.

**Five things that cost an evening, so that they do not cost another one:**

- **An address that ACKs while every command NAKs means VCC, not the bus.**
  Verified 2026-09-09, and it cost most of a night. The log said these two
  things at once, every cycle, and they read as a contradiction:

  ```
  WARN - no SHT31-D at 0x44 or 0x45
  INFO - I²C scan: 0x44 answered
  ```

  The sensor's **VCC wire was loose**. With the supply open the chip still
  draws a trickle through the ESD diodes on SDA and SCL, which sit on the bus
  pull-ups to 3V3 — enough to run the address comparator and return an ACK,
  nowhere near enough to execute a command or a conversion. The scan writes
  zero bytes (address only) and passes; the probe writes address plus two
  command bytes and fails. The fault lives exactly in that gap.

  Everything that looked strange follows from it: the sensor recovered
  spontaneously at 22:36:53 while the load-cell readings went unsteady — the
  wire making momentary contact as the node was handled — and only this node
  was affected, because it is a wiring fault and not a driver one. Swapping the
  sensor fixed it; the first full reading set in the session arrived at
  23:29:28 (25.5 °C, 43.1 %).

  **So: when a device answers its address but nothing else, check its supply
  before suspecting the bus, the pull-ups or the driver.** A dead bus fails the
  scan too, which is the distinction the scan exists to draw.

  Recorded against a wrong turn, for honesty: this was first blamed on the
  SHT31's soft reset never being issued (`CMD_SOFT_RESET` was defined and
  unused). That change shipped anyway — it is correct and datasheet-conformant
  — but it is **not** what fixed this, and flashing it changed nothing.
- **The tare baseline lives in RTC RAM** and is taken on the first boot after a
  power cycle. The beam has to be left completely alone for that boot. Taring
  while handling it captured a loaded state and left the node 38k ticks off —
  far outside the drift band and far below the threshold, so every poll landed
  in `Decision::Unexplained` and the baseline was, correctly, never absorbed.
  There is no way out of that except another power cycle.
- **A battery node's USB port enumerates only in flashes.** The earlier note
  here said it does not enumerate at all; that was wrong. It appears for a
  fraction of each 2 s poll cycle. Polling `/dev/ttyACM*` every **20 ms** and
  reading it passively catches whole log lines, including the heartbeat window
  where Wi-Fi comes up; polling every 200 ms mostly misses. Verified
  2026-09-06. Flashing still needs the boot window right after plugging in, or
  the BOOT/RESET hold, and `espflash monitor` is still the wrong tool — see
  [`FLASHING.md`](../FLASHING.md).
- **Never debug this node on USB with the cell disconnected.** Verified
  2026-09-09, after an evening spent on the wrong three theories. The LiPo is
  what buffers the radio's TX bursts; without it the board browns out and
  resets at the first transmission, and the failure is silent in the worst way:
  the serial log ends on `Wi-Fi modem sleep: max`, which `main.rs` prints
  immediately *before* `connect_async().await`. So output stops at the first RF
  current peak, the USB port vanishes, and the board reboots — with **no**
  `Connected to Wi-Fi` and, crucially, **no `connect to … failed (attempt N)`
  either**. Nothing is failing to join; it never gets that far. The SHT31-D
  fails the same way at the same time, logging the contradictory pair
  `no SHT31-D at 0x44 or 0x45` and `I²C scan: 0x44 answered`. Reattach the cell
  and both come back at once: 4.09 V, 24.5 °C, 38.9 %.

  Two hours went into the credentials before that. They were never the problem
  — the boot banner reads `wifi: 'wifi_42_ext' (built in)`, `.env` agrees, the
  network scans at signal 84 on 2.4 GHz, and nothing is stored in flash, so
  `clear` is a no-op. That banner is printed before the console window and is
  almost impossible to catch, since the C3 discards serial output no host is
  reading yet; it took five cold boots to see it once.
- **Discovery used to be announced once per *power cycle*, and that was not
  something power could be relied on to reset.** A boolean in RTC RAM said
  "already announced"; it survived deep sleep, a reflash, a reset — and, as the
  living-room node showed on 2026-09-09, several seconds with the USB cable out.
  Its entities were simply missing while its readings kept arriving. Discovery
  is now gated on a digest of every message it would send
  (`discovery::announcement_tag`), so a changed entity set re-announces itself
  and a stale word in RTC RAM fails to match by construction.

**Reflashed 2026-09-13**, from `develop`, to pick up the one-second minimum on a
visit (`deeafed`) and the I²C failure reporting (`afb34db`). Two things it
settled:

- **The visit counter survived the reflash: `visits` came back at 219.** That is
  the first real test of the `VISIT_COUNT` / `VISIT_CHECK` pair from `36b5e36`,
  against the 2 345 324 652 that leftover RTC memory produced before it existed.
  The pair is only a defence while both words are already on the board; a board
  flashed from *before* `36b5e36` gains `VISIT_CHECK` as fresh garbage, the two
  disagree, and the counter correctly reads zero and restarts. Either outcome is
  fine and neither needs intervention — but they look different in Home
  Assistant, so know which one to expect.
- **A battery node held awake on the bench publishes every `idle_secs`, i.e.
  every two seconds by default.** `sample_period_secs` takes the idle interval
  for a battery profile, and the stay-awake loop publishes every round. Off its
  beam the node also reports nonsense weight (−456.6 g here, with an
  `Unexplained` warning each round, because the RTC baseline is from the
  mounted state). Both go straight into Home Assistant *and* into QuestDB's
  three-year retention, and `smarthome/terrasse/temperature` is the outdoor
  series — so bench-testing this node indoors writes room temperature into it at
  two-second resolution. Set `deep_sleep` back to `1` as soon as the flashing is
  done, not when the node goes back outside.

After remounting on the beam, press **Tarieren** once with the feeder empty: the
RTC baseline is from the previous mounting and no remount reproduces the beam's
preload exactly. The button publishes retained, so a sleeping node collects it
on its next round — up to ten minutes away. Press it once and wait.

---

## `wohnzimmer` — mains

**Verified on hardware 2026-09-05**, all three sensors and both buses:

- **SHT31-D** at `0x44`.
- **SDS011** on `D3`/`D10`, crossed correctly. Both raw and humidity-corrected
  values arrive, and the correction is visibly doing something — the corrected
  figures sit below the raw ones, which is what the κ term should do at that
  humidity. It needs the SHT31 on the same board, which is the whole reason the
  particulate sensor lives here rather than in the kitchen.
- **SCD41** at `0x62` — verified with a unit **borrowed from
  `schlafzimmer`, which has since gone back**. The slot stays on: the fleet
  plan says this node has one, there simply is no second unit yet. Its three
  entities read unavailable in Home Assistant until one arrives.

**When assembling:** keep the SHT31-D away from the board. On this node that
matters twice over, because the particulate humidity correction reads *its*
humidity — a board-warmed SHT31 reports a relative humidity that is too low, the
correction then subtracts too little, and the error lands in the PM figures.

The SDS011's fan needs a clear air path. Its 15-minute cadence protects the fan
and the optics; a sealed enclosure would have it measuring the enclosure.

---

## `kueche` — mains, duty-cycled

SHT31-D only, verified 2026-09-04.

Duty-cycled since 2026-09-13, together with `bad`, after the new printed boxes
came in at least 2.5 °C high — see *A separate chamber is not the same as
separate air* below. Two changes went together and both need reflashing and
reprinting to take effect: the firmware now deep-sleeps between rounds, and the
`climate_tray` has vents in the board bay. Cadence is unchanged at 120 s.

Expect the boot log to say `mains, duty-cycled profile`, and expect a
**Deep Sleep** switch to appear on the device card in Home Assistant — it did
not have one before. Leave it on; `0` holds the node awake for bench testing and
is exactly the state that sat retained on the broker for days further up this
page.

---

## Open across the fleet

- **The reset diagnostics are live on `terrasse` only**, verified 2026-09-17:
  `reset_reason 21` (the USB reset espflash itself causes) and `reset_count 1`,
  unchanged across two heartbeats, so deep-sleep wakes correctly do not count.
  The other four still run without them.

  It took three attempts, and the middle one is worth knowing about. The counter
  first published **3319124736** — RTC fast RAM is not zeroed on power-up, so
  the count started from garbage. An epoch tag, the shape `discovery_tag` uses,
  *should* have fixed that and did not: the next publish read the old nonsense
  plus one, meaning the tag compared equal on the first boot of firmware that
  had only just introduced the constant. **That is still unexplained.** What
  ships now is a plausibility bound on top of the tag — past a million the count
  is discarded whatever the tag says — which is crude but cannot be defeated by
  an accidental match.

- **The whole fleet reports why it last restarted.** Added 2026-09-16, after
  the outdoor node twice went silent mid-cadence with a healthy cell and came
  back only once power was removed entirely. Two entities per node:

  | Topic | Meaning |
  | --- | --- |
  | `<node>/reset_reason` | code of the last boot that was **not** a deep-sleep wake |
  | `<node>/reset_count` | how many such boots since power was last removed |

  The codes are the hardware's own `SocResetReason` discriminants, published as
  numbers because the QuestDB archiver stores values as doubles and a string
  would not survive the trip. Decimal is what lands in the archive; hex is what
  the boot banner prints:

  | dec | hex | Meaning |
  | --- | --- | --- |
  | 0 | — | nothing but deep sleep since power-on — **the healthy reading** |
  | 1 | 0x01 | power on: the cell was disconnected, or the protection board cut |
  | 3 | 0x03 | software reset of the digital core |
  | 5 | 0x05 | deep-sleep wake — routine, never latched, so it never appears |
  | 7 | 0x07 | **main watchdog 0** — the app hung and was rebooted |
  | 8 | 0x08 | main watchdog 1 |
  | 9 | 0x09 | RTC watchdog |
  | 15 | 0x0F | **brownout** — the supply collapsed |
  | 16 | 0x10 | RTC watchdog, core and RTC together |
  | 18 | 0x12 | super watchdog |
  | 21 | 0x15 | USB UART reset — a host attached, e.g. `espflash` |
  | 22 | 0x16 | USB JTAG reset |

  **How to read it when a node goes quiet**, which is the whole point:

  - **7, 9, 16 or 18 appearing regularly** — it hangs often and usually recovers
    by itself; the silences are the times it did not.
  - **15** — the supply collapses under radio load. On a cell reading 3.7 V that
    means an aged cell with high internal resistance, not a flat one.
  - **21 or 22** — someone attached a cable. Expected after flashing, and worth
    recognising so it is not mistaken for a fault.
  - **still 0 across a silence** — a true hang: the core stopped and the
    watchdog did not fire either. That is the 2026-09-16 case, and it is the one
    with no explanation yet. Only removing power recovered it — pulling USB does
    nothing while the cell is connected, because the board is never unpowered.

  `reset_count` is what turns a single event into a trend: one is ambiguous, a
  count climbing over days is a node rebooting in a loop nobody has noticed.
  Both come out of RTC RAM, which survives a watchdog reset, a brownout and a
  software reset — so the evidence outlives the event it describes.
- **The whole fleet reports `rssi`.** The four indoor nodes were flashed
  2026-09-10 (`bad` −58, `kueche` −49, `schlafzimmer` −49, `wohnzimmer` −58 dBm)
  and `terrasse` followed on 2026-09-13 at −51. All five sit in the top bar, so
  the indicator has still not been read against a link that is actually weak —
  which is the case it exists for. `wohnzimmer` alternates −58/−61 and therefore
  straddles the three-bar threshold (`>= -61`), so its tile will flicker between
  three bars and two.

  Flashing is safe for the calibration: `espflash` reported `Segment at address
  '0x0' has not changed` and the same for `0x8000` on all boards, and the config
  blob lives at `0x9000` in the `nvs` partition, which a plain `espflash flash`
  never touches. The boot log confirms the layout:
  `boot: 0 nvs WiFi data 01 02 00009000 00006000`.
- **A newly announced entity loses its first reading.** Not a fault, but it
  cost an hour of misdiagnosis: `bad` showed `sensor.bad_signal` as
  `unavailable` while temperature and humidity published normally, and the
  serial log proved the node had read and published the value. Home Assistant
  had not finished creating the entity when the non-retained QoS0 state
  arrived. See the module docs in `src/discovery.rs`. On a battery node that gap
  lasts a full round, i.e. ten minutes.

- **Nothing has had its current measured.** Every battery figure in
  [`base-platform.md`](base-platform.md) is estimated from datasheets around a
  single measured number (a 269 ms boot). Both radio options recorded there stay
  parked behind that measurement, including the cheap one.
- **Solar for `terrasse`: panel and charger ordered 2026-09-08**, nothing built.
  Waveshare 18 V / 10 W panel and a Soldered CN3791 MPPT board (SKU 333136).
  **The board charges at 3 A as shipped** — `R8` has to come off and be replaced
  with 1.2 Ω before it is ever connected to a cell. Parts list and reasoning in
  [`solar.md`](solar.md). It depends on the same missing measurement as the
  radio options, and on the battery divider actually having been flashed —
  that divider is the only instrument that can say whether the panel works.
- **The stored-credential fallback could not fire on a battery node.** Fixed
  2026-09-17. `wifi::FALLBACK_AFTER` sets aside stored credentials after three
  consecutive refusals and falls back to the built-in pair, so a typo at the
  console cannot strand a board. But `refusals` was a task local in `main.rs`,
  and its comment said that was deliberate — *"a power cycle should give them
  another try"*. True for a mains node; a battery node cold-boots every few
  seconds, so every wake was a new run with `refusals = 0` and three was never
  reached. The protection was inert for precisely the node that hangs outdoors
  and cannot be reflashed casually.

  The counter now lives in RTC RAM beside the reset diagnostics, sharing their
  epoch tag, so it accumulates across sleeps. A **power-on** still clears it,
  which is what the original comment meant; a watchdog reset or a brownout does
  not, because those are the node failing rather than someone asking for another
  try. Three host tests cover the distinction.

  Never bit anyone: `terrasse` has nothing stored, so the boot banner reads
  `wifi: 'wifi_42_ext' (built in)` and there was nothing to fall back *from*. It
  was a trap set for the first time someone provisions the outdoor node over the
  console and mistypes.
- **Mains nodes self-heat.** Measured 2026-09-04 on `schlafzimmer`: about 0.9 °C
  at the board, separated from room warming by using the unmoved SCD41 as a
  control. Mount temperature sensors away from the board on any node that
  reports one.
- **A separate chamber is not the same as separate air.** Reported 2026-09-13 on
  `kueche` and `bad` after the new printed boxes went in: at least 2.5 °C high,
  which is a lot more than the 0.9 °C above. The boxes do have two compartments
  with a baffle between them, so "away from the board" was already satisfied on
  paper. What was missing was anywhere for the board's heat to go. The board bay
  had no vents at all; the only openings in the box were at the sensor end, and
  the baffle's full-height wire notch is 168 mm² — so the board's entire heat
  load was drawn *through* the sensor chamber on its way out. The baffle was not
  separating the two volumes, it was aiming one at the other.

  Two fixes, both landed: the board bay gets its own chimney in `models.py`
  (inlet low, two outlets high, both long walls, 225 mm² — the notch is left
  alone, because the terrasse box already lost a baffle to a jumper housing that
  would not thread through a narrower one), and both nodes moved to
  `PowerProfile::MainsDutyCycled` so the board is off between rounds.

  The lid was left solid in that first pass, on the argument that nothing in
  this box needs room air the way the `schlafzimmer` SCD41 does. That answers
  the wrong question: the board does not need room air, it needs its heat gone,
  and the wall outlets sit at `z = 19` of a 24 mm bay, so the warm air has to
  turn to reach them. The lid now carries a 187 mm² grille over the board bay,
  on the same `x` bounds as the wall slots, and a 125 mm² one over the sensor
  chamber — the second is not about heat but about response time, since every
  other opening into that end is a side slot the room air has to turn to come
  through.

  **Both cuts rest on a fact about the rooms, not about the box.** An open lid
  in a kitchen or a bathroom is where grease and condensate get in, and the
  sensor is the part that never reports its own failure: a wet RH sensor still
  returns a number. What makes it affordable is that `kueche` and `bad` sit on
  a shelf, not under a hob or a shower head. Re-check that before printing this
  design for a third room; if the next one has to hang somewhere exposed, the
  sensor grille is the cut to leave out.

  The slots stop short of `x = 6` on purpose. The jumpers to the SHT31 loop
  through the 8.5 mm between board and baffle, and slots that reach the board
  edge put a 2.5 mm opening at jumper-housing height right where a wire is
  handled. A wire fits through one. Keep the corridor blank when adding vents to
  any box in `models.py`.

  **Check the humidity, not just the temperature.** RH is read against
  temperature, so a box 2.5 °C warm reports RH about seven points low. If the
  fix worked, both numbers move, and the humidity moving is what proves it was
  the air and not the sensor.
