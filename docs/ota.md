# Updating the fleet over the air

What it would take to stop walking round the house with a USB-C cable, what is
already in place, and the two things that can turn a firmware update into a
board that has to be fetched off a mast.

## Why

The rollout on 2026-09-17 is the argument. Five nodes, five cable trips, about
half an hour of wall-clock — and one of them went wrong in a way that is only
possible with a cable: a `wohnzimmer` image ended up on the `terrasse` board,
which then published eleven hours of living-room data under the wrong name
before anyone noticed. That failure is written up in
[`commissioning.md`](commissioning.md) and [`annotations.md`](annotations.md);
what matters here is that **the mistake is a property of the process, not of the
person** — `NODE=` is set per invocation, the ELF path is the same for every
node, and nothing between the shell and the flash checks that the image matches
the board.

An over-the-air path replaces that with a message naming a node and an image
built for it. It also reaches the one node where a cable is genuinely expensive:
`terrasse`, on a mast, in an enclosure, on a battery.

## What already works

**The bootloader can do this today.** We boot through the ESP-IDF second-stage
bootloader — `ESP-IDF v5.1.2-342-gbcf1645e44`, visible in every boot log — and
it has supported A/B application slots and an `otadata` selector for years. None
of this is a bootloader port; it is a partition table, a flash writer and a
state machine.

**Flash writing is already in the firmware.** `esp-storage` is a dependency and
`src/config.rs` uses it on every configuration change: the node reads and writes
its own sectors while the radio is up. Whatever is true about writing flash on
this chip under load, this firmware has been doing the small version of it for
weeks.

**The transport half exists too.** `embassy-net` gives us TCP, the home server
already runs nginx, and the node already learns where to look — the broker's
address is baked in and `NTP_SERVER` defaults to it, because the machine that
runs mosquitto is the machine that runs everything else.

## The blocker: the boards in the house have no OTA partition layout

Until 2026-09-17 the repository shipped no partition table, so `espflash` used
its default: a single `factory` application, no second slot, no `otadata`. That
is what every board in the house is running now, and the boot log says so:

```
I (59) boot:  0 nvs              WiFi data        01 02 00009000 00006000
I (67) boot:  1 phy_init         RF data          01 01 0000f000 00001000
I (74) boot:  2 factory          factory app      00 00 00010000 …
```

A new table lives at `0x8000`, which is exactly the region `espflash` reports as
*unchanged* on every flash we do. So **the first OTA-capable image still has to
go on by cable** — one last trip round the fleet, five boards, and after that
the trips stop.

### The constraint that makes this delicate

The firmware does not use the `nvs` partition as NVS. It writes three fixed
sectors inside that range, from `src/config.rs`:

| Offset | What it holds | Lost if overwritten |
| --- | --- | --- |
| `0x9000` | the configuration blob | scale calibration, tare, intervals, `deep_sleep` |
| `0xA000` | the node identity | which room a provisioned board thinks it is |
| `0xB000` | stored Wi-Fi credentials | the fallback pair set at the console |

A partition table that moves or shrinks that region below `0xC000` costs the
fleet its calibration — the `terrasse` load cell above all, which is the one
value in the house nobody can reproduce from memory. Every layout below keeps
all three sectors where they are.

### The table

`partitions.csv`, which every cabled flash now writes — the `cargo run` runner
in `.cargo/config.toml` passes `--partition-table`, and `espflash` reports it
back as a table rather than a default.

4 MB of flash and an application of 738 528 bytes, so two slots fit with room to
spare — each slot holds 2.7 images:

```csv
# Name,     Type, SubType,  Offset,    Size
nvs,        data, nvs,      0x9000,    0x4000
otadata,    data, ota,      0xD000,    0x2000
phy_init,   data, phy,      0xF000,    0x1000
ota_0,      app,  ota_0,    0x10000,   0x1F0000
ota_1,      app,  ota_1,    0x200000,  0x1F0000
```

- `nvs` keeps its start and still spans `0x9000`–`0xD000`, so all three blobs
  stay addressable at their current offsets. It shrinks from 24 KB to 16 KB,
  which costs nothing: the firmware uses 12 KB of it and none of it as NVS.
- `otadata` needs exactly two sectors — the bootloader keeps two copies and
  picks the one with the higher sequence number, which is what makes the switch
  atomic across a power cut.
- Both application slots are 1 984 KB, and the image fills **36 %** of one.
  Equal sizes on purpose: asymmetric slots mean an image can fit the slot it is
  written to and not the one it would be written to next, which is a failure
  that only shows up on the update after next.
- `0x3F0000`–`0x400000` is left unassigned. 64 KB is not worth the loss of round
  numbers.

There is deliberately **no `factory` partition**. With only OTA slots, an empty
or corrupt `otadata` makes the bootloader fall back to `ota_0`, which is where
the cabled migration writes. A `factory` partition would add a third image that
nothing ever updates and that would silently become the oldest firmware in the
house.

## The transport: MQTT decides, HTTP carries

The obvious design is to push the image through the broker. It is the wrong one
here, for reasons specific to this house:

- **The archiver would see it.** It subscribes to `smarthome/+/+` — exactly
  three segments — so anything published as `smarthome/<node>/ota` lands in the
  ingest path and is counted as an unparseable reading. Chunks one level deeper
  would miss it, but the trigger would not.
- **The fleet's ACL would have to grow.** The `birdscale` user is restricted to
  `smarthome/#`, `birds/#` and `homeassistant/#` precisely so a compromised node
  cannot reach the rest of the broker. A dedicated `ota/#` tree means widening
  that.
- **Retained chunks are a liability.** An image left retained is re-delivered to
  every subscriber on every connect — Home Assistant and the archiver included —
  and forgetting to clear it is a permanent 740 KB tax on every reconnect.
- **Someone has to write the chunk protocol.** Ordering, gaps, resume and
  back-pressure over an at-least-once channel is a real protocol, and it already
  exists in HTTP.

So MQTT carries the *decision* and HTTP carries the *bytes*:

```
smarthome/<node>/ota/offer     {"version":"kueche-425e2c4",
                                "url":"http://192.168.1.67/fw/kueche-425e2c4.bin",
                                "sha256":"…64 hex…","size":738528}
smarthome/<node>/ota/version   kueche-425e2c4          (retained, by the node)
```

**Four levels deep, not three**, which is the detail that decides whether this
is invisible to the archiver: it subscribes to `smarthome/+/+`, so
`smarthome/<node>/ota` would land in the ingest path and be counted as an
unparseable reading on every delivery. Anything under `ota/` misses that filter
entirely.

The version topic is the other half of the deal. A node saying which commit it
is running is the answer to the question that cost an hour on 2026-09-17, and
`version` is `<node>-<commit>` rather than a bare hash on purpose: **a node
refuses an image whose version does not name it**, so the wrong-image mistake
that started this cannot be made over the air.

Retained, so a sleeping node collects it on its next wake — the same mechanism
every runtime knob already uses. The node fetches over plain TCP from the home
server, verifies the digest, and writes the inactive slot as the bytes arrive.
Resume after a lost association is a `Range:` header, and the state lives on the
node rather than on the broker.

The URL is LAN-only and the digest is integrity, not authenticity. Anyone who
can publish to `smarthome/#` can already provision a board into another room and
change a scale's calibration; an image they can also serve is not a new class of
access. Signing is the right answer the day the broker is reachable from
outside, and not before.

## Rollback, which is the part that matters

A node that boots a broken image and cannot be reached again is the whole risk,
and it is concentrated in one node — `terrasse`, on the mast. Two rules:

**Confirm on evidence of usefulness, not on boot.** An image with a broken Wi-Fi
path, a wrong broker address or a panic after association boots perfectly and is
never heard from again. The confirmation is therefore *a completed publish
round*: MQTT connected, readings sent. Nothing else counts.

**Do the confirming in the application.** ESP-IDF's automatic rollback is a
*bootloader* build option (`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`), and the
bootloader we ship is the prebuilt one that comes with `espflash` — we do not
control its configuration, and an `otadata` state of "pending verify" is very
likely to be ignored by it. Assuming otherwise would produce exactly the failure
this section exists to prevent: a fleet that believes it can roll back and
cannot.

So the application does it, using what this codebase already does elsewhere —
a counted, checked value that survives a reset:

1. After writing the new slot, mark it *unconfirmed* and point `otadata` at it.
2. Before each publish attempt, an unconfirmed slot increments a counter in
   flash.
3. A completed publish round clears the counter and marks the slot confirmed.
4. Past three attempts without a confirmation, rewrite `otadata` to the previous
   slot and reset. The old image is still there — that is the point of A/B.

**Per publish attempt, not per boot**, and that distinction was the one real
design mistake caught while building this. A battery node cold-boots out of
deep sleep every few seconds and only brings the radio up on its heartbeat, so
counting boots would burn three attempts inside a minute and roll back an image
that had never been given a chance to publish at all. Counting attempts to
reach the broker gives `terrasse` three heartbeats — half an hour — and a mains
node three rounds.

The counter is incremented *before* the attempt rather than after it, so an
image that panics halfway through publishing still counts against itself. That
is the failure mode the whole mechanism exists for, and it is the one that would
otherwise never record anything.

Three attempts rather than one because the failure this protects against is a
bad *image*, not a bad afternoon: a router reboot should not roll back a working
update.

## The nodes that sleep

A duty-cycled or battery node is awake for seconds, which is not a download. The
switch for that already exists and is already documented: publish
`smarthome/<node>/config/deep_sleep` as `0`, retained, wait for it to take, run
the update, publish `1` again. `kueche` has been living on that switch since
2026-09-14 for unrelated reasons, which is incidentally why it was the easiest
board in the house to reflash by cable.

For `terrasse` the sequence costs a few minutes of the ~28 mAh/h that being
awake draws. That is the correct trade against a trip up a mast, and it is worth
having the update path refuse to start below a battery threshold rather than
discovering the cell was flat halfway through writing a slot.

## What Home Assistant gets

The MQTT discovery spec has an `update` component, so the fleet can announce its
own update entities exactly the way it announces everything else — nothing
declared by hand, which is the rule this project has kept so far. Installed
version and available version as state, an install button as command. The
version string wants to be `<NODE>-<git-describe>`, stamped in by `build.rs`
alongside the credentials it already re-runs on: a node that cannot say
precisely which commit it is running turns "is this one updated?" back into the
guessing game that cost an hour this evening.

## Known unknowns

Written down because they are the things that would make this harder than it
looks:

- **Writing flash while the radio is up.** `esp-storage` disables the cache for
  each operation, and on this chip anything executing from flash during that
  window is a fault. `config.rs` gets away with it for a handful of sectors;
  an image is about 190 erase-and-write cycles. The mitigation is small chunks
  between network reads rather than one long write, and the honest position is
  that this needs measuring on hardware before the first battery node is
  trusted to it.
- **`esp-hal` is pinned at 0.22.** Community OTA crates track the current
  release; whether one of them compiles against 0.22 is a question to answer by
  trying, not by reading a version table. The `otadata` format itself is stable
  and small enough to write directly — sequence number, label, state, CRC-32 in
  two sectors — and this codebase already has the CRC-32 it needs.
- **RTC RAM does not survive a slot switch any better than it survives a
  reflash.** The `terrasse` tare baseline was lost this evening because a
  different image laid out its persistent words differently. The same applies to
  an OTA update between images whose persistent layout changed, and the existing
  defence — a checked magic pair, `scale::VISITS_MAGIC` — is the pattern to
  extend, not to trust as already covering it.

## What is built, and what is not

Written 2026-09-17, in the evening after the cabled rollout, and updated the
same night: it has since **run on hardware** — see *The first trial* below.

Built:

- `partitions.csv`, and `.cargo/config.toml` passes it on every flash.
- `src/ota.rs` — the `otadata` selector (the bootloader's own CRC rule, which is
  *not* the CRC the config blobs use), slot selection, the image writer with its
  sector staging, the offer parser, and the attempt/rollback state machine.
  Its constants are checked against `partitions.csv` by a test that reads the
  file with `include_str!`, so the two cannot drift apart.
- `src/sha256.rs` — streaming, hand-rolled, pinned to the canonical vectors.
- `src/http.rs` — one `GET`, `Range` resume, and a body handed to a sink a
  sector at a time. Generic over the stream, so the whole download path is
  exercised on the host against canned responses: a 404 that arrives with a
  perfectly valid HTML body, a server that ignores `Range`, a connection that
  dies mid-header.
- `build.rs` stamps `FW_VERSION` as `<node>-<commit>`, `-dirty` when the tree
  does not match, and the node publishes it retained on `ota/version`.
- The publish round subscribes to `ota/offer`, installs what it finds, and
  reboots into it.

Not built, and deliberately:

- **The Home Assistant `update` entity.** The discovery payloads gain an entity
  and therefore a new digest, which re-announces the whole fleet; worth doing
  once the mechanism has been exercised rather than in the same change.
- **Resume across rounds.** `http::fetch` takes a `from` offset and the
  writer could carry one, but nothing yet remembers a partial download across a
  reboot. A failed download currently starts again from zero, which on a mains
  node costs seconds.
- **A server side.** Nothing publishes offers or serves `/fw/`. That is an nginx
  location and a script that writes a JSON object; it belongs in `home-server`,
  not here.

## Order of work from here

1. **The cabled migration.** One trip round the fleet with
   `--partition-table partitions.csv`. Nothing else can be tested without it,
   and it is the last time the cable is needed.
2. **Serve one image and offer it to `wohnzimmer`** — mains, always awake,
   reachable in ten seconds, and the least costly board in the house to get
   wrong.
3. **Watch what writing 740 KB does with the radio up.** This is the known
   unknown above, and it is the one that decides whether a battery node is ever
   allowed to do this.
4. **Then `terrasse`**, which is the node this whole thing is for, and only
   after the switch has been watched to work and to roll back.

## The first trial

`wohnzimmer`, 2026-09-17, 23:36. The whole path, on hardware, from an offer on
the broker to a node running the image it named.

The image was served from a laptop by `python3 -m http.server`, with no
server-side change at all: the offer's URL only has to be a LAN address, which
is exactly why the downloader refuses hostnames instead of carrying DNS. The
node is on mains and stays associated, its USB port is therefore stable, and the
serial log below is a passive read — no `espflash monitor`, which would have
reset the chip to attach.

```
update offered: wohnzimmer-a958878-dirty (768272 bytes) -> slot 1
768272 bytes written to slot 1, digest matches
image written and selected (seq 2); restarting into it
node 'wohnzimmer' (Wohnzimmer) booted, mains profile
running an unconfirmed image (attempt 1 of 3); it is confirmed by reaching the broker
update confirmed: this image has published a round
```

The board reported `slot 1`, `seq 2` afterwards, and the archiver shows it back
on the air within about one round — a minute, for a 60-second cadence.

**The open question is answered, for a mains node.** Writing 768 KB with the
radio up — 188 sectors, each an erase and a write with the cache disabled — did
not disturb the association or the executor. It is still unanswered for a
battery node, which is a different power budget and a different supply.

**Four things went right that are worth keeping:**

- The first four attempts *failed*, because the laptop's firewall trusts only
  the tailnet and the node is on the LAN. Each failure cost one log line and
  nothing else — no partial state, no reboot — and because the offer is
  retained, the node retried on its own once the port was opened. Nothing had to
  be republished.
- The image was written to **slot 1** while slot 0 was running, so every failed
  attempt was a write to a slot nothing would boot.
- After the reboot the still-retained offer was refused with *"offer names the
  version already running"*. Without that check the node would have installed
  the same image on every round, for ever. It is the difference between a
  retained offer and a reboot loop.
- The image was confirmed only after a completed publish round, not on boot.

**What the trial did not cover**, and therefore what is still only as good as
its host tests:

- **Resume.** `python3 -m http.server` ignores `Range` and answers `200`, which
  the node correctly refuses on a resumed request — so the resume path was never
  exercised end to end. A real server, nginx included, would answer `206`.
- **Rollback.** Nothing failed, so nothing rolled back. The attempt counter and
  the selector rewrite have only ever run in tests.
- **A battery node.** `terrasse` is the node this exists for and the one that
  has not done it yet.

**A second update followed at 23:58**, to `wohnzimmer-fa931e5` — a real commit
this time rather than a `-dirty` tree — and it is the one that proves the part
the first could not: it went to **slot 0**, back where the cabled image had
been, with `seq 3`. The slots alternate, the sequence number only climbs, and a
node that has been updated twice is running from the slot it started in. Nothing
about the second run needed a cable, a button or a person in the room.

## The fleet, over the air

2026-09-18, 08:32 to 08:55: all five nodes went from `…-fa931e5` to
`…-c0b36ee` without anyone touching a board. The images were served by the home
server — `server.firmwareServer` in `home-server`, an nginx `location` and a
directory — rather than off a laptop, which changes three things:

- **Resume is reachable at last.** nginx answers `Range` for a static file:
  `curl -r 100-199` returns `206` with `Content-Range: bytes 100-199/768256`,
  where a Python `http.server` answered `200` and the node correctly refused it.
- **The fleet has no DNS**, so the vhost carries `192.168.1.67` in its
  `server_name` beside `fw.home.arpa`. Without the literal a node reaches
  whichever vhost nginx treats as the default, and the failure reads as "server
  refused the request".
- **Nothing depends on a laptop being open**, or on a hole in its firewall.

Two firsts worth separating. `bad` was the first node updated **through the
deep-sleep path** — `wohnzimmer` is a mains node and had only ever exercised the
stay-associated loop — and it was done to a board on a bathroom wall, out of
sight. And `terrasse` is the first **battery** node to update itself: it wakes,
measures, publishes, fetches, writes, restarts, and confirms on its next
heartbeat. That is the case this whole mechanism was built for, and the one
where a cable means a ladder.

An update leaves `reset_reason 3` behind — the software reset the node performs
on itself to boot the new slot. It is worth knowing as a *signature*: 3 after an
offer is the mechanism working, while 7, 15 or 21 in the same place would be a
watchdog, a brownout or somebody with a cable.

Afterwards each retained offer was withdrawn (`-r -n`) and the firewall hole
closed. Both matter: an offer left on the broker is re-delivered on every
connect for ever, and it is the one piece of this mechanism that outlives the
session that created it.
