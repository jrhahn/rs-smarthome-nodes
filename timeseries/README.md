# `smarthome-timeseries`

The fleet's long-term memory: one process that follows the MQTT topics the
nodes already publish, writes every reading into **QuestDB** with a **three-year
retention**, and serves a small dashboard that reads it back.

```
┌──────────┐   smarthome/<node>/<key>   ┌───────────────┐   ILP/HTTP   ┌──────────┐
│  nodes   │───────────────────────────▶│   this        │─────────────▶│ QuestDB  │
│ (ESP32)  │   homeassistant/…/config   │   service     │◀── SQL ──────│ readings │
└──────────┘───────────────────────────▶│               │              │ + 3 views│
                                        └───────┬───────┘              └──────────┘
      ┌────────────────┐                        │  HTTP :8087
      │ Home Assistant │◀── same broker ────────┘  charts, live values, health
      └────────────────┘
```

Nothing here touches the firmware or Home Assistant's configuration. A node does
not know it is being archived; switching this off leaves the house working
exactly as before.

## Why it exists

Home Assistant already sees every node — the firmware announces itself over MQTT
auto-discovery — and it keeps a recorder database. What it does not keep is
*years*. Its recorder is tuned for weeks, and the questions this fleet was built
to answer are the slow ones: did insulating the roof change the bedroom's
overnight CO₂, how does the terrace swing between summers, is the bird feeder
busier this year than last.

So: a second consumer on the same broker, whose only job is not to lose
anything.

## What it stores, and what it does not

The firmware's topic layout is the entire schema:

| Topic | Stored as |
| --- | --- |
| `smarthome/<node>/<key>` | a row in `readings` |
| `smarthome/<node>/status` | a row in `node_status`, **on change only** |
| `smarthome/<node>/config/<key>` | nothing — a setting, not an observation |
| `smarthome/provision/<mac>` | nothing — an instruction |
| `homeassistant/sensor/<node>/<key>/config` | nothing; read for units and names |

Two details that are easy to get wrong, and are tested:

- **`provision` is not a node.** `smarthome/provision/a1b2c3d4e5f6` has the same
  three segments as a reading, and would otherwise be archived as a node called
  `provision` with a sensor called `a1b2c3d4e5f6`.
- **A retained reading is not a new reading.** The broker replays retained
  messages when this service subscribes, and the only timestamp available here
  is "now" — so a replay would stamp an old value as current. Readings arriving
  with the retain flag are skipped. This does not affect live publishes: MQTT
  clears the flag on messages delivered to an already-subscribed client, so a
  node publishing retained data is still archived normally.

Timestamps are when the bridge received the message. The nodes send none of
their own — they are `no_std` boards without a clock — and on a broker on the
same LAN the gap is milliseconds.

## The database

```sql
readings(timestamp TIMESTAMP, node SYMBOL, sensor SYMBOL, value DOUBLE)
  TIMESTAMP(timestamp) PARTITION BY DAY TTL 3 YEARS
```

plus three cascading materialized views, each aggregating the next finer one:

| View | Bucket | Reads | Partition |
| --- | --- | --- | --- |
| `readings_1m` | 1 minute | `readings` | day |
| `readings_1h` | 1 hour | `readings_1m` | month |
| `readings_1d` | 1 day | `readings_1h` | year |

Each stores `min`, `max`, `sum` and `count` per channel per bucket. The mean is
re-derived at read time as `sum(sv)/sum(n)` — the exact mean of the underlying
readings, not an average of averages that would weight a sparse hour (node
asleep, one reading) like a full one.

**Why cascade.** QuestDB refreshes a view by re-aggregating the buckets the new
rows touched. A daily view built directly on the base table therefore re-scans
the whole current day on every commit, for ever. Built on the hourly view it
re-reads twenty-four rows per channel.

**Why not timers.** The gateway firmware puts its coarse tiers on `REFRESH EVERY
5m` / `1h`, because at 100 Hz ingest an immediate refresh of every tier is what
pegged three CPU cores. This fleet publishes about one reading a minute per
channel, so there is nothing for a timer to save — and on QuestDB 9.3.5 a
timer-refreshed view created over a source that was still empty did not pick up
2160 rows inserted into that source over the following twenty minutes. That is
exactly the shape of a first install. Every tier here is `REFRESH IMMEDIATE`.

**Retention** is a QuestDB table TTL, applied to the base table and to every
view, and re-applied on each start — so changing `retention` in the settings
file is enough to change it on a table that already exists. QuestDB drops data a
whole partition at a time, which is why the setting must be a whole number of
days or coarser.

## Reading it back

`GET /api/series?node=…&sensor=…&from=…&to=…&points=…` picks a bucket from a
fixed ladder (1s … 30d), then routes the query to the **coarsest view whose
bucket is still fine enough**. Below a minute, and whenever a view is missing,
it reads the base table. The response is identical either way — `lo`, `hi`, `av`
per bucket — so the chart never branches on where the numbers came from.

Two things make that promise literal rather than approximate:

- The window is **widened to whole buckets** before querying. Ask for "the last
  365 days" at a daily grain and the window starts mid-day: the raw table would
  answer with a partial first bucket while the daily view's row for that day
  starts before the window and is filtered out entirely. Rounded out, both
  answer with the same full bucket.
- A rollup that returns nothing is **retried against the base table**, so a view
  that is somehow behind shows the truth rather than a blank chart.

This is checked against a live database rather than asserted: see the smoke test
below, which compares every zoom level against a full scan of the raw table.

| Endpoint | |
| --- | --- |
| `GET /` | the dashboard (three embedded files, no CDN, no build step) |
| `GET /api/overview` | every channel at once: labels, last value, and a sparkline |
| `GET /api/series` | one channel, full resolution |
| `GET /api/annotations` | the notes in a window; `POST` writes one |
| `GET /api/channels` | the channel list without any series — for scripts |
| `GET /api/health` | broker and database reachability, rollups, counters |

`/api/overview` is one database query for the whole screen rather than one per
channel. Thirty-one round trips to paint a page would be the obvious way and the
wrong one; the rollups are already grouped by channel, so the same `SAMPLE BY`
without a `node`/`sensor` filter answers all of them together.

## What the dashboard shows

Two views, and the split is the useful part.

**The overview** is a wall of tiles, one per channel, grouped by node: the
current value, its unit, a sparkline over the selected range, and the span that
sparkline covers. Small multiples rather than one chart with thirty-one lines
on it — the channels are in different units, and a shared axis across
micrograms, parts per million and degrees would be a lie. Node headings carry an
online/offline word beside the dot, because green and red are precisely the pair
a colour-blind reader cannot separate.

**The detail view** is one channel at full size: the min/max band with the mean
through it, min / mean / max / last as numbers above it, a crosshair that reads
out the bucket under the pointer, and the same numbers as a table for anything a
curve reads badly. Time ticks land on the hour, midnight or the first of the
month rather than on the span divided by eight — evenly spaced ticks put
`10.09.` on the axis twice on a week-wide chart.

Both views live in the URL: `#c/<node>/<sensor>/<range>` is a chart worth
sending to someone, and reloading keeps it.

## Annotations

A series outlives the memory of the hardware that produced it. The terrace's
weight steps because the scale was recalibrated, not because a heavier bird
arrived; the living room's VOC index climbs from 1 for a day because the
algorithm is learning the room. None of that is in the numbers.

`annotations` is a table beside the readings — timestamp, node, one sentence —
and the notes are drawn on the chart at the moment they describe. **It carries
no TTL at any setting**: a note that expires before the data it explains is
worse than no note at all.

The dashboard's *Notiz …* button writes one. `docs/annotations.md` in this
repository keeps the same history in prose and is the better place for the long
version; this is the half that a chart can draw.

## Configuration

One TOML file, every field optional — see
[`timeseries.example.toml`](timeseries.example.toml). Defaults assume a broker
and a QuestDB on localhost. Passwords can come from `password_file` so they stay
out of the file (and, on NixOS, out of the store).

## Building and running

The firmware's toolchain pin does not apply here; this crate has a
`rust-toolchain.toml` of its own and a `.cargo/config.toml` that undoes the
riscv32 default.

```bash
nix develop .#timeseries      # nixpkgs rustc, plus questdb and mosquitto
cd timeseries
cargo test                    # 75 tests, no database needed
cargo clippy --all-targets -- -D warnings
cargo run -- timeseries.example.toml
```

### The end-to-end smoke test

The unit tests cover the parts that are pure computation — topic classification,
ILP formatting, DDL shape, tier routing, bucket snapping. What they cannot cover
is whether QuestDB accepts any of it. That needs a real database:

```bash
# a QuestDB and a broker of their own, on ports nothing else uses
mkdir -p /tmp/ts-smoke/qdb/conf
cat > /tmp/ts-smoke/qdb/conf/server.conf <<'EOF'
http.bind.to=127.0.0.1:19000
http.min.net.bind.to=127.0.0.1:19003
pg.net.bind.to=127.0.0.1:18812
line.tcp.net.bind.to=127.0.0.1:19009
EOF
questdb.sh start -n -d /tmp/ts-smoke/qdb &
printf 'listener 11883 127.0.0.1\nallow_anonymous true\n' > /tmp/ts-smoke/mosquitto.conf
mosquitto -c /tmp/ts-smoke/mosquitto.conf &

# then point a settings file at 11883 / 19000 and run the service against it,
# publish with mosquitto_pub, and compare a rollup-answered chart against a
# full scan of the base table.
```

Doing exactly that is what turned up the two edge cases the code now handles
(partial buckets at a window's edge, and coarse views left empty by a timer).

## On the home server

The flake exposes a package and a NixOS module. The module also brings up
QuestDB itself, because nixpkgs ships the package but no service:

```nix
{
  inputs.smarthome-nodes.url = "github:jrhahn/rs-smarthome-nodes";

  # in the host's configuration:
  imports = [ inputs.smarthome-nodes.nixosModules.smarthome-timeseries ];

  services.smarthome-timeseries = {
    enable = true;
    settings = {
      mqtt.host = "127.0.0.1";       # the broker Home Assistant already uses
      questdb.retention = "3y";
      web.bind = "127.0.0.1:8087";
    };
    # Kept out of the store; systemd hands it over as a credential.
    mqtt.passwordFile = "/run/secrets/mqtt-password";
  };
}
```

The archiver runs as a `DynamicUser` with no state of its own. QuestDB gets a
static user and `/var/lib/questdb`, which is **the one directory worth backing
up** — it holds the data, the write-ahead log and the configuration.

Both QuestDB ports bind to loopback by default. That is deliberate: QuestDB's
HTTP interface serves ingestion, arbitrary SQL *and* a web console, none of it
authenticated.

> **Upgrades are one-way.** QuestDB's storage format only moves forward: a data
> directory written by one version cannot be opened by an older one, so rolling
> the package back after an upgrade does not roll the data back with it.

## Operations

**Change the retention.** Edit `questdb.retention`, restart. The service issues
`ALTER TABLE … SET TTL` on every start, for the base table and for every view.

**Look at the data directly.** QuestDB's console is on port 9000 (loopback, so
over an SSH tunnel), and the PostgreSQL wire protocol on 8812 for anything that
speaks it — Grafana, `psql`, a notebook.

**After changing a view's definition** in `rollup.rs` or `schema.rs`: the DDL is
`IF NOT EXISTS`, so an existing view is left alone. Drop the affected views
(coarsest first) and restart; they backfill from the history on creation.

```sql
DROP MATERIALIZED VIEW readings_1d;
DROP MATERIALIZED VIEW readings_1h;
DROP MATERIALIZED VIEW readings_1m;
```

**If a view is ever behind**, `REFRESH MATERIALIZED VIEW readings_1h INCREMENTAL`
catches it up without a full rebuild. `SELECT view_name, view_status,
refresh_base_table_txn, base_table_txn FROM materialized_views()` is how to tell.

## How long to keep it, and what that costs

Measured rather than guessed, and written up in
[`docs/long-term-history.md`](../docs/long-term-history.md): the fleet produces
~25,900 readings a day, a finished daily partition of exactly that shape came to
**828 KB**, and three years of raw data is therefore under a gigabyte. That page
also argues why the rollups should outlive the raw table rather than expire with
it, and what threatens a long series once storage has stopped being the problem.

## Limits worth knowing

- **Readings are lost if the database is unreachable for a long time.** The
  writer retries a failed batch on the next flush and keeps up to 100 000 rows
  buffered — over a day of this fleet's output — then drops the backlog with a
  loud log line rather than growing until the machine swaps.
- **No authentication on the dashboard.** It is read-only and binds to
  loopback; anything more is the reverse proxy's job.
- **The service does not create the MQTT user.** It is a normal client; give it
  read access to `smarthome/#` and `homeassistant/#`.
