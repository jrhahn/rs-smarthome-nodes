# Keeping the fleet's data for years

What it costs to keep every reading, what the retention should actually be, and
the three things that threaten a long series once storage stops being the
problem. The service that does the keeping is
[`timeseries/`](../timeseries/README.md); this is the reasoning around it.

## What the fleet produces

Thirty-one channels, at the cadences `node.rs` configures:

| Node | Channels | Every | Per day |
| --- | --- | --- | --- |
| `schlafzimmer` | temperature, humidity, co2, scd41_temperature, scd41_humidity, rssi | 60 s | 8,640 |
| `wohnzimmer` | the same six minus co2's siblings, plus voc_index, nox_index | 60 s | 11,520 |
| `wohnzimmer` | pm25, pm10, pm25_raw, pm10_raw | 900 s | 384 |
| `kueche`, `bad` | temperature, humidity, rssi | 120 s | 2,160 each |
| `terrasse` | weight, visits, temperature, humidity, battery ×2, rssi | ~600 s | ~1,000 |

**≈ 25,900 readings a day.**

## What that costs on disk

Measured rather than estimated: three days of exactly that shape were written
into QuestDB 9.3.5 and the finished daily partition read back.

```
2026-09-10    25,872 rows    828 KB    ≈ 32 bytes/row
```

Thirty-two bytes is what the columns come to — an 8-byte timestamp, two 4-byte
symbol references, an 8-byte double — plus the symbol index. So:

| | Raw | With the rollups (estimated) |
| --- | --- | --- |
| 1 year | 0.3 GB | ~0.8 GB |
| 3 years | 0.9 GB | ~2.4 GB |
| 10 years | 3 GB | ~8 GB |

The rollup figure is arithmetic, not measurement: `_1m` holds about as many rows
as the base table with four more columns per row, and `_1h` (≈740 rows/day) and
`_1d` (31 rows/day) are noise beside either.

> **Do not read the size off the active partition.** QuestDB pre-allocates
> append space, so `table_storage()` reported 81 MB for the day being written
> while the finished neighbour beside it was 828 KB. The number only settles
> when the partition closes.

## What follows: three years is too short

`retention = "3y"` was chosen before any of this was measured. At 0.3 GB a year
it is not a storage decision, and the rollups exist for *speed*, not for space —
a full-range chart reads `_1d` either way.

**The intended shape is raw for a few years, rollups for ever.** A year-on-year
comparison in 2032 reads 31 rows a day; there is no reason for that to expire.
That needs one change to the service: retention is currently a single value
applied to the base table and every view alike, and it should be per tier.

## What actually threatens a long series

### Gaps, when the archiver is not running

The nodes publish at **QoS 0** (`main.rs`). A QoS 0 message is never queued for
an absent subscriber — not with a persistent session, not with a QoS 1
subscription, because delivery happens at the lower of the two. Every restart of
the archiver, every deploy, every crash is therefore a hole in the series, and
nothing on the broker side can close it.

Fixing it properly would mean publishing at QoS 1, which costs a battery node an
extra round trip per reading — the wrong trade for the node that is hardest to
keep alive. So the mitigations are ordinary ones: run the archiver on the same
host as the broker, keep restarts short, and remember that Home Assistant's
recorder is a second subscriber holding the last ~10 days independently.

A hole is at least visible: the chart simply has no points there.

### Backups that are not backups

`/var/lib/questdb` is the directory that matters, and copying it while the
database is running is a coin flip — the WAL may be mid-apply. QuestDB has
`SNAPSHOT PREPARE` and `SNAPSHOT COMPLETE` for exactly this: prepare, let the
backup run over the directory, complete. Without that pair, a restore is
untested by construction.

This is the only item on this page whose absence goes unnoticed until the day it
matters.

### A series nobody can interpret in three years

The data will outlive the memory of what happened to the hardware. The step in
the terrace's weight on 2026-09-09 was a recalibration. The CO₂ offset changed
when an SCD41 was replaced. The living room's VOC index starts at 1 and climbs
for a day because the algorithm is learning the room, not because the air is
improving.

None of that is in the numbers. A small `annotations` table — timestamp, node,
one sentence — or even a dated list in this repo is what turns a pile of
readings into a measurement series. It is the cheapest thing here and the one
most likely to be skipped; [`annotations.md`](annotations.md) is where it is
being kept until the table exists.

## The order of work

1. **Deploy the archiver.** It is built and verified against a real QuestDB, and
   runs nowhere. The NixOS module is in `timeseries/nix/module.nix`; the home
   server needs the flake input, the module import, an MQTT password as a
   secret, and a proxy entry. Everything below is hypothetical until this is
   done.
2. **Retention per tier** — raw 3 years, rollups without expiry.
3. **Snapshot-aware backup** of `/var/lib/questdb` in the existing borg run.
4. **Annotations** — started, by hand, in [`annotations.md`](annotations.md). A
   table in the database is still the better home for them, so a chart can show
   them; a dated list costs nothing and is already worth more than the memory it
   replaces.
5. Optionally, once the archive has been running a while: trim Home Assistant's
   recorder to ~10 days. Its job becomes the live view; the history lives here.
