# Base sensor platform — design notes

Tracking: **#11** (epic) with sub-issues #12–#17.

The firmware started as a single-purpose bird-feeder scale. It is now one
configurable base for several ESP32-C3 nodes, each carrying a different subset
of sensors, selected with `NODE=<name>` at build time.

## Fleet

| `NODE=` | Node | Sensors | Power profile |
| --- | --- | --- | --- |
| `draussen` (default) | Draußen | load cell (HX711) + DS18B20 + SHT31-D | battery, deep-sleep |
| `schlafzimmer` | Schlafzimmer | SCD41 + SHT31-D | mains, always-on |
| `wohnzimmer` | Wohnzimmer | SCD41 + SHT31-D + SDS011 | mains (fan) |
| `kueche` | Küche | SHT31-D | mains |
| `bad` | Bad | SHT31-D | mains |
| `terrasse` | Terrasse | none yet — every slot off while it is wired up | battery, deep-sleep |

A node's sensors need not share a cadence: `sample_secs` is the base round, and
a slot may ask for a slower one of its own (`Slot::every`). `wohnzimmer` is the
case that forced it — a 60 s round for the SCD41, and one round in fifteen for
the SDS011, whose fan is a ~8000 h consumable.

Per-node wiring — which sensor sits on which pad, with the pull-ups and supply
notes — is in [wiring.md](wiring.md).

## Sensor set

| Sensor | Bus | Emits | Status |
| --- | --- | --- | --- |
| HX711 load cell | bit-bang | force → grams | done (`hx711.rs`) |
| DS18B20 | 1-Wire | temperature | done (`ds18b20.rs`) |
| SHT31-D | I²C 0x44 | temperature, humidity | done (`sensors/sht31.rs`, #13) |
| SCD41 | I²C 0x62 | CO₂, temperature, humidity | done (`sensors/scd41.rs`, #14) |
| SDS011 | UART 9600 | PM2.5, PM10 | done (`sensors/sds011.rs`, #15) |

I²C addresses do not clash, so SHT31-D + SCD41 share one bus. SDS011 is the only
UART sensor, and on `wohnzimmer` it runs alongside that bus. HX711 stays a
blocking bit-bang critical section.

The SDS011's readings are corrected for humidity where a node also carries an
SHT31 (`Slot::compensated`): a nephelometer over-reads in damp air, so the
κ-Köhler growth factor is divided back out and both the corrected and the raw
values are published. κ is a Home Assistant slider, because it is a property of
the room's aerosol rather than of the sensor.

> **Bench status:** every driver is implemented and the whole fleet builds, but
> the I²C/UART drivers have not yet been run against real hardware — the bus
> timing and the SDS011 warm-up in particular want a session on the bench.

## Abstraction (#12)

`sensors::Sensor` is HAL-agnostic:

- `measure() -> Vec<Reading>` — one measurement; empty on absence (never fatal).
- `descriptors() -> &[EntityDescriptor]` — per-reading metadata for HA discovery.

`Reading` carries a `key` (topic suffix) + a pre-formatted, float-free `value`,
matching the existing on-device formatting. Shared helpers live in
`sensors/mod.rs`: `crc8_sensirion` + `crc_word` (SHT31/SCD41), `write_tenths`,
`write_int`.

The drivers are generic over the `embedded-hal-async` (I²C) and
`embedded-io-async` (UART) bus traits, so they name no esp-hal type.
`platform.rs` is the one place that does: it brings up only the buses this node
needs, wraps the single I²C peripheral in a `SharedI2c` handle (a
`NoopRawMutex` — everything is sequential on one executor) so both I²C drivers
can *own* their bus as the trait wants, and exposes one `measure_all()`.

`node.rs` holds the per-node table: identity (id/name/namespace), the sensor
slots, the power profile and the mains sampling cadence. A slot can rename its
readings (`prefix` / `label`) so two sensors emitting the same quantity don't
collide — the outdoor node's DS18B20 keeps `temperature` while its SHT31-D
publishes `air_temperature` / `air_humidity`. An unknown `NODE` value fails the
build in const-eval.

### Which node am I? (build-time default + NVS override)

The identity is resolved once at boot, before any peripheral is touched, since
the sensor set decides which buses come up at all:

1. `BUILT_AS` — the `NODE=` the image was compiled with (default `draussen`);
2. a name stored in flash, which overrides it.

`node::active()` returns the result. It is a plain `Copy` struct read from a
`static`, written once by `node::init()` — the same single-context discipline
`state.rs` uses for its RTC-RAM cells, which is sound here because nothing runs
before `init()` and nothing writes after it.

The override is set over MQTT on `smarthome/provision/<mac>` (retained), keyed
by MAC because that is the only name an unprovisioned board is sure of. A valid,
*different* name is written to flash and the board restarts into it; anything
else — an unknown name, or the name it already answers to — is ignored without
touching flash, which matters because a retained message is re-delivered on
every single connect.

The identity blob lives in the **second** NVS sector (`0xA000`), separate from
the calibration blob at `0x9000`: provisioning must never be able to disturb a
scale's tare, and the two have completely different write frequencies. Same
discipline as the config blob — magic, version, length, CRC-32 — so a blank,
erased, corrupt or half-written sector reads back as "no override" and the board
falls back to what it was built as.

The publish path iterates whatever the node produced (`Samples`) instead of
hard-coded HX711/DS18B20 calls. The HX711 and DS18B20 keep their own read paths
— the first because its reading drives the tare baseline and presence edge, the
second because its 750 ms conversion is only worth spending on publish cycles —
and contribute their readings and discovery descriptors to the same pipeline.

### Three ways a visit used to go missing

The presence decision moved out of `main` into
[`presence`](../src/presence.rs) once it became clear that the one piece of
arithmetic deciding whether a visit is recorded *at all* was also the only part
of the firmware no host test could reach. Two of the three holes it had are now
closed; the third is a physical trade-off and stays open.

**Absorbed by the drift tracker.** The baseline nudged itself by `delta/16` on
*any* sub-threshold reading, which is right for slow thermal and mechanical
creep and wrong for a bird. A visitor whose load sat below `threshold` — a small
species, or one perched half on the rim — was pulled into the baseline within
roughly 16 cycles, so the scale read "empty" again while it was still standing
there. On departure the delta went *negative*, also sub-threshold, so no falling
edge fired either and the baseline crept back. The visit left no trace in the
logs or on the broker, and the failure was systematic rather than random: it
discriminated against light birds specifically. Drift is now confined to a band
of `threshold/4`; anything between that band and the threshold is
`Decision::Unexplained`, which leaves the baseline alone and says so in the log.

**Detected, then badly weighed.** On a rising edge the firmware used to publish
one arbitrary conversion and deep-sleep for `active_interval` (default 10 s) —
*slower* than the 2 s idle poll, so the cadence dropped exactly when dense data
was wanted. One unaveraged sample could land while the bird was still settling,
the departure was quantised to the same 10 s grid, and every one of those cycles
paid its own Wi-Fi connect: a two-minute visit cost about twelve of them. A
visit is now watched through awake with the radio off — cheap, since visits are
short — sampling the cell continuously into a 32-deep ring. The published weight
is the median of that ring, which is deliberately the *most recent* samples: a
bird that has just landed is still moving, so the tail of a visit describes it
better than the head. `VISIT_MAX` caps one awake window at 60 s so a load that
is not a bird (snow, a twig) cannot hold the CPU awake; such a load falls back
to the old `active_interval` polling, which is all that knob is still for.

**Shorter than the idle interval.** A visit that begins and ends between two
polls is invisible, and with `idle_secs: 2` plus the ~0.3 s cold boot that
window is real. This one is not a bug and is not fixed: the HX711 has no
threshold output that could wake the ESP32-C3 from deep sleep, so the only
control is the wake rate — which is itself the dominant battery cost on this
node, well ahead of the radio. Lowering `idle_interval` trades runtime for
short-visit coverage, and nothing in the firmware can make that trade for you.

The published visit duration is a **lower bound** for the same reason: the
arrival is only known to within one idle interval.

### Where the battery actually goes

The node was built around "deep sleep is free, the radio is expensive", and
that turned out to have the ranking backwards. Two things dominated instead,
and neither was the radio.

**The boot overhead.** Deep sleep sat inside the poll loop, so a 2 s idle poll
cost a full ROM boot plus app init — 269 ms measured — to clock out one 100 ms
conversion. At `idle_secs: 2` that is roughly a quarter of the node's life
spent booting, against the ~18 s an hour the radio was actually up. The polls
now run inside one boot with light sleep between them, so a cold boot happens
once per *publish*: about six an hour rather than eighteen hundred.

**The amplifier.** The HX711 and the bridge it excites draw continuously —
about 1.5 mA for the chip plus whatever the load cell's bridge takes, which for
a 1 kΩ cell at ~3.2 V excitation is another ~3 mA. `power_down()` and
`power_up()` had been written, documented and then never called, because there
was no sleep that retained the pad level: in deep sleep the pad goes
high-impedance and `PD_SCK` is left to leakage, which is also why the
amplifier's deep-sleep current was never a known quantity. The amplifier is now
powered down between polls, and the datasheet's 400 ms settling wait after
power-up is spent *in light sleep* rather than awake — the amplifier needs to be
on for it, the CPU does not, and waiting it out awake would have cost more than
the power-down saves.

The two changes compose: light sleep is what makes the power-down possible,
because it retains GPIO output levels. RTC pad hold on `SCK` (issue #5) was only
ever necessary because deep sleep was the steady state. It no longer is — deep
sleep now lasts one poll interval per publish, purely so the next cycle gets a
fresh `Radio`, since `publish` consumes it.

**These are estimates, not measurements.** The only measured figure in the
paragraphs above is the 269 ms boot time, read off a real serial log. The
current figures come from datasheets, and the two that would move the answer
most are the load cell's bridge resistance — a 350 Ω cell triples that term —
and the light-sleep current with this chip's `RtcSleepConfig` default, which
keeps the digital domain powered and so does not match the datasheet's headline
light-sleep number. A multimeter in series with the cell, once asleep and once
awake, settles both.

## MQTT auto-discovery (#16)

On the first connect after a power-up, each node publishes one retained
`homeassistant/sensor/<node>/<key>/config` per reading, so Home Assistant creates
the device (identifiers = node id) and all its entities. State topics are
`<namespace>/<node>/<prefix><key>`; the config/command topics stay
`<namespace>/<node>/config/<key>`.

"Once per power cycle" is a flag in RTC RAM: the messages are retained, so
re-sending them on every deep-sleep wake would only cost battery, while a cold
power-on re-announces exactly when the broker may have lost them.

Power cycle means power cycle. RTC fast RAM survives a reflash and the reset
that follows it, so flashing a board does not re-announce its entities — pull
the power for that.

The bird scale keeps its historical `birds/scale/…` namespace, and its weight is
still mirrored to the pre-discovery `birds/scale/state` topic, so the migration
of the hand-declared Home Assistant entities can happen at leisure.

### Command entities

The knobs that flow the other way — the ones Home Assistant publishes to
`<namespace>/<node>/config/<key>` — are discovered as well, as `number`,
`switch` and `button` entities under the device's *Configuration* category. They
used to be the one thing still hand-declared in YAML, which meant each node in
the fleet needed its own copy-pasted block.

Which knobs a node gets follows from what it *is*: the calibration ones only on
a node with a load cell, the sleep/interval ones only on a battery node, since a
mains node samples on its build-time cadence and never sleeps. A dead control on
a device card is worse than a missing one.

Two details that are not obvious:

- **The button consumes its own message.** A button's payload is a constant, so
  the firmware cannot tell one press from the next; and the message must be
  retained, because a battery node is asleep when the button is pressed. So the
  node deletes the retained message (empty retained payload) once it has
  re-zeroed. The older timestamp-token scheme still works and is still
  remembered, which doubles as a backstop if a delete is ever lost.
- **`stat_t` points back at the command topic.** The node never echoes its
  stored config, so the sliders would otherwise sit at "unknown" until moved.
  Since commands are retained, the command topic already holds the last value
  set — which is exactly the state an optimistic control would show, except this
  one survives a Home Assistant restart.

### Availability

Two mechanisms, because neither alone covers a fleet that is half asleep:

| | Mains node | Battery node |
| --- | --- | --- |
| Last will (`avty_t` → `<ns>/<node>/status`) | yes | **no** |
| `expire_after` | 3 × `sample_secs` | 3 × `heartbeat_secs` |

A last will catches the node that dies *while connected*: the broker publishes
retained `offline` and Home Assistant greys the entities out at once. The node
publishes retained `online` right after each connect, and — importantly — sends
a proper MQTT `DISCONNECT` when a round is done, which tells the broker to
discard the will. Without that, a mains node that reconnects per round would be
declared dead after every single publish.

Battery nodes get **no** will at all: they are legitimately disconnected almost
all the time, so a will would mark them offline seconds after every reading.
They rely on `expire_after` instead, which is also what catches a mains node
that dies *between* rounds (a clean disconnect discards the will, so nothing
else would notice). Three missed rounds is the threshold — one miss is a lost
packet or a failed join, three is a node that stopped — with a 120 s floor.

`expire_after` is derived from the live config, so changing the heartbeat
interval clears the "discovery published" flag and re-announces the entities
with the new expiry.

## Power profiles (#17)

- **battery**: deep-sleep between samples (the bird-scale behaviour).
- **mains**: stay awake with Wi-Fi up, sample + publish every
  `NodeConfig::sample_secs` (60 s indoors, 900 s in the kitchen so the SDS011
  fan is only spun 4×/h). Continuous operation is required for CO₂ value and for
  the fan.

The profile is a per-node build-time choice. The live `config/deep_sleep` switch
still works, but only on a battery node — a mains node has nothing to gain from
sleeping and CO₂/PM continuity to lose, so it ignores it. The SCD41 follows the
profile too: periodic measurement on mains (what its self-calibration expects),
single-shot on battery.

### Staying awake is a thermal problem

A mains node never sleeps, so everything it burns turns into heat inside the
enclosure — and the sensors then measure their own board instead of the room.
Measured on `schlafzimmer` against a reference thermometer, 2026-09-03: the air
at the electronics sat ~1.5 °C above the room, and both the SHT31-D and the
SCD41 reported it faithfully. Worse, the SCD41's temperature offset had been
calibrated against that same warm SHT31-D, so the error had been copied into it
and the two agreeing with each other proved nothing.

Three things came out of that, in order of how much they were worth:

1. **Mount the sensors away from the board.** No firmware can undo a sensor
   sitting in warm air. This is the fix; the rest are refinements.
2. **80 MHz instead of 160.** The minimum esp-wifi will run the radio at
   (`MIN_CLOCK` in its `init`), and dynamic power scales with the clock. A round
   is a few I²C transactions and one publish, and I²C/UART timing comes off APB
   rather than the CPU clock, so nothing here notices the difference.
3. **Modem sleep, `PowerSaveMode::Maximum`.** esp-wifi never calls
   `esp_wifi_set_ps` by itself and the stack below comes up in MIN_MODEM, so
   `Minimum` would have been a no-op — `Maximum` is the setting that actually
   changes something. It costs up to ~300 ms before an inbound packet is
   collected; everything inbound here is a Home Assistant knob, and outbound
   traffic never waits on the sleep schedule.

Deliberately *not* done: a software temperature offset for the SHT31-D. The
overtemperature is roughly constant (P·R_th), so an offset is defensible in
principle, but it hides the problem rather than fixing it, depends on airflow,
and would have to correct the humidity alongside the temperature.

## Testing

`cargo test` cannot link for `riscv32imc`, and the crate used to be a single
binary that pulled in esp-hal, so it could not be built for the host either.
That left compile-time `const` assertions as the only harness, and the awkward
habit of validating things like the discovery JSON with throwaway host replicas
of the real code — copy-paste that proves nothing once it drifts.

So the crate is now a library plus a thin binary, and everything that touches a
bus, RTC RAM, flash or the radio sits behind the `hal` feature. With it off, the
remainder builds for the host: the protocol decoding, the CRCs, the flash blob
layouts, `Config::apply`, the node table and the discovery payload builders.
That is most of the logic that can be silently *wrong* rather than fail to
build.

Two changes fell out of making that possible, both of which stand on their own:

- `discovery` takes the `NodeConfig` as an argument instead of reading the
  global identity, so the payloads are a pure function of the node;
- the provisioning-payload parser moved out of `main.rs` into `node`, with the
  two facts it needs (the current identity, whether flash holds an override)
  passed in rather than read.

The tests worth having are the cross-checks, not the arithmetic: that a
discovered state topic is exactly the topic the publish path uses, that every
control key is one `Config::apply` accepts, that no two entities on a node share
a topic or a unique id, and that `KNOWN_NODES` still lists the fleet. Those are
the failures that would otherwise show up as an entity stuck at "unknown".

Compile-time assertions stay: they fail the build rather than a test run, and
they cover invariants a test cannot reach.

### Telling a dead bus from a missing sensor

A targeted probe cannot distinguish them: "no SHT31-D at 0x44" is what you get
whether the sensor is absent, at another address, or the bus is not working at
all — and those have completely different fixes. So when an expected device is
missing, and only then, the firmware sweeps `0x08`–`0x77` with zero-length
writes and reports what answered. Anything answering proves the wiring and the
pull-ups are fine; nothing answering proves they are not.

It is ~110 transactions, which is why it is a reaction to a problem rather than
part of every boot.

### Driver tests against fake buses

Making the drivers generic over `embedded-hal-async` / `embedded-io-async` was
about keeping them free of esp-hal types; the other half of that payoff is that
the **real** drivers run on the host against a scripted bus (`sensors/mock.rs`).
The `hal` feature was split for it: `drivers` brings in only the bus traits and
embassy-time, whose `std` time driver is what lets a test await a conversion.

The UART mock is the interesting one, because the SDS011 driver distinguishes
"stale frames buffered while the fan span up" from "the frame I want" purely by
draining until the line falls quiet. So the script is a list of segments with an
optional gap before each, and a run-dry mock returns `Pending` rather than
`Ok(0)` — which is what a real UART does, and what lets the driver's timeout
fire. Covered: the full duty cycle, the fan being parked on *every* exit path,
resync past noise, a rejected checksum, an absent sensor, and a stray `0xAA`
costing exactly one frame.

Writing them turned up one real hazard. `read_byte` and `drain` looped on a
read that completed with zero bytes, which never yields — so the surrounding
timeout could never fire and the driver would spin for ever. esp-hal's UART does
not do that (it waits for a byte) but the trait permits it, so both loops are
now bounded, with a `StarvedUart` test to keep them that way.

The SCD41 gets the same treatment, and there the two run modes are the point.
Periodic (mains) must send `start_low_power_periodic_measurement` exactly
**once** — a node that re-sent it every round would restart the conversion cycle
and never read anything — and must poll data-ready before reading, or it gets
the previous measurement. The low-power cadence (one sample per 30 s) rather
than the 5 s one: no mains node publishes faster than every 60 s, and the
measurements nobody collects are paid for in heat next to the SHT31-D. Single-shot (battery) does neither: it asks for one conversion,
waits it out, reads. Both must treat `0 ppm` as "no measurement yet" rather than
as air, which would otherwise draw a plausible flat line at zero. That one
single-shot test really does sit through the datasheet's ~5 s conversion; unlike
the SDS011 warm-up it is a fixed sensor timing, not a policy knob, so there is
nothing honest to shorten.

The HX711 gets the same treatment through the `embedded-hal` pin traits, and it
is the one driver that has actually been running — which is precisely why the
bit protocol is worth pinning down. The fake pins share a line, so a test can
check not only *what* was read but *when*: 24 bits most-significant-first, each
sampled while the clock is high (sampling on the low phase reads the next bit on
real hardware and produces plausible numbers rather than obvious ones), 25/26/27
total pulses depending on gain, the clock parked low afterwards so the chip is
not latched into power-down, and a disconnected amplifier timing out rather than
returning noise as a weight.

What this does *not* cover: timing, bus contention, anything electrical. A green
test says the driver handles the bytes correctly, not that the SHT31 answers
within 15 ms on the real bus.

## SDS011 warm-up

The fan cannot run continuously (~8000 h rated), so it is woken per sample — and
how long it stays awake was, until recently, a flat 20 s. That is a guess in
both directions: too long for still air, and too short for air that is actually
changing, which is exactly when the reading matters.

It now serves a **floor** and then watches. The floor (10 s) is not optional:
before the airflow establishes, the sensor repeats its last value, so frames
agree with each other perfectly and a settling check alone would stop on air
that is not moving yet. After it, the driver reads frames until three in a row
agree — within 1 µg/m³ plus 5 % of the larger, an absolute floor for clean air
where the sensor's own noise dominates and a proportional band for dirty air
where it does not — on **both** particle sizes, since PM2.5 settling while PM10
still climbs is not a settled reading.

A ceiling (30 s) stops it there and reports whatever it has: unsettled air is a
reason to publish the latest number, not to keep the fan spinning. One frame is
always read however tight the budget, because an unsettled reading beats an
empty round.

In still air this typically ends around 13 s rather than 20 — roughly 7 s of fan
life back on every sample, which on the kitchen node's four samples an hour is
the difference the duty cycle exists for.

## Wi-Fi credentials

Credentials were compile-time only, which made the one failure that matters
unrecoverable: a board that cannot join the network cannot be told anything
over the network either. MQTT provisioning — how the node identity is set — is
no help, because reaching the broker is precisely what is broken.

So the escape hatch is the **serial console**, the one channel that still works.
On a **cold boot** the board listens briefly for `ssid` / `psk` / `save`, stores
the pair in its own flash sector and restarts into it; `clear` returns it to
whatever it was flashed with. Physical access already implies the ability to
reflash, so this gives away nothing a USB cable did not.

Three decisions worth recording:

- **Cold boot only.** RTC RAM is wiped by a power-up and survives deep sleep, so
  the existing flag distinguishes "someone just plugged this in" from "this woke
  up to take a reading". A battery node pays the window once per power cycle
  rather than every two seconds.
- **The window depends on how stuck the board is.** Three seconds when it has
  usable credentials — it is only offering the chance to intervene — and two
  minutes when it does not, since a board that cannot join has nothing better to
  be doing.
- **Stored credentials are not trusted blindly.** After three consecutive
  refusals the connection task falls back to the build-time pair for the rest of
  the run. Unlike a wrong node name, a wrong passphrase cannot be corrected over
  the air, so without this a single typo would take a board off the network
  until someone walked over with a cable. The count deliberately lives in RAM,
  not RTC RAM: a power cycle should give the stored pair another try, because
  the likeliest cause of a run of failures is an access point that was down.

The passphrase is taken verbatim after the keyword, so one containing spaces
survives; only the line ending and trailing whitespace are stripped. A mangled
passphrase would be indistinguishable from a wrong one, which is a bad evening.
`show` prints the SSID and the passphrase's *length*, never its content — those
lines go to a log that may be scrolling in someone else's terminal.

## Known gaps

- Console provisioning is the only way to *set* credentials; there is no way to
  rotate them across the fleet remotely. Changing the router's passphrase means
  visiting each board with a cable.
