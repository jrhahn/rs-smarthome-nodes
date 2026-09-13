//! The read side: turning "draw me this channel over this range" into one SQL
//! statement, and the answer into something a chart can consume.
//!
//! Two rules shape everything here.
//!
//! **The response does not say where it came from, except as a label.** A
//! bucket answered from `readings_1d` and the same bucket answered from the raw
//! table carry the same numbers -- `min`, `max` and a count-weighted mean --
//! so the frontend never branches on the source. The `source` field is there
//! for the footer and for debugging, not for logic.
//!
//! **The mean is re-derived, never re-averaged.** Each rollup row stores a sum
//! and a count, so a coarser bucket is `sum(sv)/sum(n)`: the exact mean of the
//! underlying readings. Averaging the finer tier's averages would weight a
//! sparse hour (one reading, node asleep) the same as a full one (sixty).

use anyhow::{bail, Result};
use serde::Serialize;

use super::client::{as_f64, as_i64, as_str, Client};
use super::rollup::{self, Tier};
use crate::model::Micros;

/// The bucket widths a chart is allowed to ask for, finest first.
///
/// Snapping to a ladder rather than using `range / points` directly buys two
/// things: buckets line up with the calendar (a "1h" bucket starts on the
/// hour), and panning a chart re-uses the same bucket boundaries instead of
/// shifting them by a pixel's worth of time, which would make the same data
/// look subtly different on every drag.
const LADDER: &[(Micros, &str)] = &[
    (1_000_000, "1s"),
    (5_000_000, "5s"),
    (10_000_000, "10s"),
    (30_000_000, "30s"),
    (rollup::MINUTE, "1m"),
    (2 * rollup::MINUTE, "2m"),
    (5 * rollup::MINUTE, "5m"),
    (10 * rollup::MINUTE, "10m"),
    (15 * rollup::MINUTE, "15m"),
    (30 * rollup::MINUTE, "30m"),
    (rollup::HOUR, "1h"),
    (2 * rollup::HOUR, "2h"),
    (3 * rollup::HOUR, "3h"),
    (6 * rollup::HOUR, "6h"),
    (12 * rollup::HOUR, "12h"),
    (rollup::DAY, "1d"),
    (2 * rollup::DAY, "2d"),
    (7 * rollup::DAY, "7d"),
    (30 * rollup::DAY, "30d"),
];

/// The finest ladder step that still keeps the result under `points` rows.
pub fn choose_bucket(from: Micros, to: Micros, points: i64) -> (Micros, &'static str) {
    let span = (to - from).max(1);
    let wanted = span / points.max(1);
    LADDER
        .iter()
        .find(|(width, _)| *width >= wanted)
        .copied()
        .unwrap_or(*LADDER.last().expect("ladder is not empty"))
}

/// A value that is going into a SQL string literal.
///
/// Node ids and sensor keys arrive as query parameters, so they are attacker-
/// controlled in the same sense that anything reachable on the LAN is. Rather
/// than escaping quotes and hoping, this refuses anything that is not the shape
/// a topic segment actually has. QuestDB has no bind parameters over the HTTP
/// endpoint, so the check has to happen here.
pub fn checked_literal(raw: &str) -> Result<&str> {
    if raw.is_empty() || raw.len() > 120 {
        bail!("identifier {raw:?} is empty or too long");
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
    {
        bail!("identifier {raw:?} contains characters that a node or sensor name never has");
    }
    Ok(raw)
}

/// One bucket of a chart.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Point {
    /// Bucket start, in milliseconds -- what `new Date(t)` wants.
    pub t: i64,
    pub lo: f64,
    pub hi: f64,
    pub av: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Series {
    /// The table the numbers came from, for the chart's footer.
    pub source: String,
    pub bucket: &'static str,
    /// The window actually covered, in milliseconds -- the requested one
    /// widened to whole buckets (see [`snap`]). The chart draws its axis from
    /// this rather than from what it asked for, so the first and last bars are
    /// not half off the edge.
    pub from: i64,
    pub to: i64,
    pub points: Vec<Point>,
}

/// Widen a window to whole buckets: `from` down, `to` up.
///
/// Without this, a rollup and the raw table genuinely disagree at the edges,
/// and the disagreement is not a rounding error. Ask for "the last 365 days"
/// at a daily bucket and the window starts in the middle of a day: the raw
/// table answers with a partial first bucket (the hours after `from`), while
/// the daily view's row for that day starts *before* `from` and is dropped by
/// the filter entirely. One source shows a short bar, the other shows none --
/// from the same data. Rounded out to whole days both answer with the same
/// full bucket.
///
/// Every bucket on the ladder is a fixed multiple of a second, and QuestDB's
/// `ALIGN TO CALENDAR` anchors those at the epoch, so flooring in epoch
/// microseconds lands exactly on its boundaries.
pub fn snap(from: Micros, to: Micros, width: Micros) -> (Micros, Micros) {
    let floor = from.div_euclid(width) * width;
    let ceil = to.div_euclid(width) * width + if to.rem_euclid(width) == 0 { 0 } else { width };
    (floor, ceil.max(floor + width))
}

/// Build the statement for one chart.
///
/// `tier` of `None` means the base table, which is both the deep-zoom case and
/// the fallback when no view exists.
pub fn series_sql(
    base: &str,
    tier: Option<&Tier>,
    node: &str,
    sensor: &str,
    from: Micros,
    to: Micros,
    bucket: &str,
) -> String {
    let (table, aggregates) = match tier {
        Some(t) => (t.view(base), "min(lo) lo, max(hi) hi, sum(sv)/sum(n) av"),
        None => (
            base.to_string(),
            "min(value) lo, max(value) hi, avg(value) av",
        ),
    };
    // The cast to `long` happens in an outer projection: inside, `timestamp`
    // has to stay the designated timestamp or SAMPLE BY has nothing to sample
    // by. Casting at all is to hand the frontend an epoch rather than an
    // ISO-8601 string it would only parse back into one.
    format!(
        "SELECT cast(timestamp AS long) t, lo, hi, av FROM (\
         SELECT timestamp, {aggregates} FROM {table} \
         WHERE node = '{node}' AND sensor = '{sensor}' \
         AND timestamp >= cast({from} AS timestamp) AND timestamp < cast({to} AS timestamp) \
         SAMPLE BY {bucket} ALIGN TO CALENDAR)"
    )
}

/// What a chart asks for: one channel, one window, and how many buckets the
/// caller can draw.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub node: &'a str,
    pub sensor: &'a str,
    pub from: Micros,
    pub to: Micros,
    pub points: i64,
}

/// Fetch one chart, routing it to the coarsest view that still resolves the
/// requested bucket.
pub async fn fetch_series(
    client: &Client,
    base: &str,
    views: &[String],
    request: &Request<'_>,
) -> Result<Series> {
    let Request {
        node,
        sensor,
        from,
        to,
        points,
    } = *request;
    let node = checked_literal(node)?;
    let sensor = checked_literal(sensor)?;
    let (width, bucket) = choose_bucket(from, to, points);
    let (from, to) = snap(from, to, width);
    let tier = rollup::pick(width, views, base);

    let mut series = run_series(client, base, tier, node, sensor, from, to, bucket).await?;

    // A rollup that answers with nothing is not proof that there is nothing: a
    // view can be behind its source, or have been recreated and not yet
    // backfilled, while the base table already holds months. Asking
    // the base table before reporting "no data" costs one query in a case that
    // is rare, and turns a blank chart into a correct one.
    if tier.is_some() && series.points.is_empty() {
        series = run_series(client, base, None, node, sensor, from, to, bucket).await?;
    }
    Ok(series)
}

#[allow(clippy::too_many_arguments)]
async fn run_series(
    client: &Client,
    base: &str,
    tier: Option<&Tier>,
    node: &str,
    sensor: &str,
    from: Micros,
    to: Micros,
    bucket: &'static str,
) -> Result<Series> {
    let sql = series_sql(base, tier, node, sensor, from, to, bucket);
    let data = client.exec(&sql).await?;
    let (ti, lo, hi, av) = (
        data.require("t")?,
        data.require("lo")?,
        data.require("hi")?,
        data.require("av")?,
    );
    let points = data
        .rows()
        .iter()
        .filter_map(|row| {
            // A bucket with no rows cannot appear at all (no FILL), but a
            // bucket whose only rows are nulls can, and a chart cannot draw a
            // null. Dropping it leaves a gap, which is the truth.
            Some(Point {
                t: as_i64(row.get(ti)?)? / 1_000,
                lo: as_f64(row.get(lo)?)?,
                hi: as_f64(row.get(hi)?)?,
                av: as_f64(row.get(av)?)?,
            })
        })
        .collect();

    Ok(Series {
        source: tier
            .map(|t| t.view(base))
            .unwrap_or_else(|| base.to_string()),
        bucket,
        from: from / 1_000,
        to: to / 1_000,
        points,
    })
}

/// One statement for every channel's recent shape.
///
/// The overview draws a sparkline per channel, and there are thirty-one of them.
/// Asking per channel would be thirty-one round trips to answer one screen; the
/// same `SAMPLE BY` without a node/sensor filter answers all of them at once,
/// because the rollups are already grouped by channel. The rows come back
/// interleaved and are split by key on this side, which is arithmetic rather
/// than work.
pub fn overview_sql(
    base: &str,
    tier: Option<&Tier>,
    from: Micros,
    to: Micros,
    bucket: &str,
) -> String {
    let (table, average) = match tier {
        Some(t) => (t.view(base), "sum(sv)/sum(n) av"),
        None => (base.to_string(), "avg(value) av"),
    };
    format!(
        "SELECT cast(timestamp AS long) t, node, sensor, av FROM (\
         SELECT timestamp, node, sensor, {average} FROM {table} \
         WHERE timestamp >= cast({from} AS timestamp) AND timestamp < cast({to} AS timestamp) \
         SAMPLE BY {bucket} ALIGN TO CALENDAR)"
    )
}

/// One channel's recent shape: its key, and the mean per bucket.
pub type Sparkline = ((String, String), Vec<(i64, f64)>);

/// `(node, sensor)` to its sparkline, for every channel with data in the window.
pub async fn fetch_overview(
    client: &Client,
    base: &str,
    views: &[String],
    from: Micros,
    to: Micros,
    points: i64,
) -> Result<Vec<Sparkline>> {
    let (width, bucket) = choose_bucket(from, to, points);
    let (from, to) = snap(from, to, width);
    let tier = rollup::pick(width, views, base);
    let data = client
        .exec(&overview_sql(base, tier, from, to, bucket))
        .await?;
    let (ti, n, s, av) = (
        data.require("t")?,
        data.require("node")?,
        data.require("sensor")?,
        data.require("av")?,
    );

    let mut out: Vec<Sparkline> = Vec::new();
    for row in data.rows() {
        let Some(key) = (|| {
            Some((
                as_str(row.get(n)?)?.to_string(),
                as_str(row.get(s)?)?.to_string(),
            ))
        })() else {
            continue;
        };
        let Some(point) = (|| Some((as_i64(row.get(ti)?)? / 1_000, as_f64(row.get(av)?)?)))()
        else {
            continue;
        };
        match out.iter_mut().find(|(k, _)| *k == key) {
            Some((_, series)) => series.push(point),
            None => out.push((key, vec![point])),
        }
    }
    Ok(out)
}

/// Every channel the database has ever seen, with the time of its last row.
///
/// Asked of the coarsest available rollup, which is the whole point of having
/// one: on the base table this is a full scan, on `readings_1d` it is a few
/// thousand rows even after three years.
pub fn channels_sql(base: &str, views: &[String]) -> String {
    let table = rollup::TIERS
        .iter()
        .rev()
        .find(|t| views.iter().any(|v| *v == t.view(base)))
        .map(|t| t.view(base))
        .unwrap_or_else(|| base.to_string());
    format!("SELECT node, sensor, cast(max(timestamp) AS long) last FROM {table}")
}

/// The same question over the last couple of days of the base table.
///
/// The coarse rollup above is the complete list of *history*, but it lags: a
/// channel that first published ten minutes ago is not in `readings_1d` until
/// that view's timer fires. Since the base table is partitioned by day, this
/// reads two partitions and no more, so it stays cheap for ever -- and a new
/// sensor shows up in the sidebar as soon as its first reading is flushed.
pub fn recent_channels_sql(base: &str) -> String {
    format!(
        "SELECT node, sensor, cast(max(timestamp) AS long) last FROM {base} \
         WHERE timestamp > dateadd('d', -2, now())"
    )
}

/// The current value of every channel, straight from the database.
///
/// `LATEST ON` is QuestDB's own answer to this and reads one row per channel
/// rather than scanning. Taking it from the database rather than from the
/// bridge's memory means a restarted service shows the fleet's state
/// immediately, instead of an empty dashboard until every node next publishes
/// -- which for a battery node is ten minutes.
pub fn latest_sql(base: &str) -> String {
    format!(
        "SELECT node, sensor, value, cast(timestamp AS long) t FROM {base} \
         LATEST ON timestamp PARTITION BY node, sensor"
    )
}

/// The last availability transition per node.
pub fn latest_status_sql(status_table: &str) -> String {
    format!(
        "SELECT node, online, cast(timestamp AS long) t FROM {status_table} \
         LATEST ON timestamp PARTITION BY node"
    )
}

/// `(node, sensor, last_seen_ms)` for every known channel: the whole history
/// from the coarse rollup, plus anything that has published in the last two
/// days and may not have reached it yet.
pub async fn fetch_channels(
    client: &Client,
    base: &str,
    views: &[String],
) -> Result<Vec<(String, String, Option<i64>)>> {
    let mut out = channel_rows(client, &channels_sql(base, views)).await?;
    // Only worth a second query when the first one asked a view; without
    // rollups both statements read the same table.
    if channels_sql(base, views) != recent_channels_sql(base) && !views.is_empty() {
        for (node, sensor, last) in channel_rows(client, &recent_channels_sql(base)).await? {
            match out.iter_mut().find(|(n, s, _)| *n == node && *s == sensor) {
                Some(existing) => existing.2 = existing.2.max(last),
                None => out.push((node, sensor, last)),
            }
        }
    }
    Ok(out)
}

async fn channel_rows(client: &Client, sql: &str) -> Result<Vec<(String, String, Option<i64>)>> {
    let data = client.exec(sql).await?;
    let (n, s, l) = (
        data.require("node")?,
        data.require("sensor")?,
        data.require("last")?,
    );
    Ok(data
        .rows()
        .iter()
        .filter_map(|row| {
            Some((
                as_str(row.get(n)?)?.to_string(),
                as_str(row.get(s)?)?.to_string(),
                row.get(l).and_then(as_i64).map(|us| us / 1_000),
            ))
        })
        .collect())
}

/// `(node, sensor, value, at_ms)` for every channel's most recent row.
pub async fn fetch_latest(client: &Client, base: &str) -> Result<Vec<(String, String, f64, i64)>> {
    let data = client.exec(&latest_sql(base)).await?;
    let (n, s, v, t) = (
        data.require("node")?,
        data.require("sensor")?,
        data.require("value")?,
        data.require("t")?,
    );
    Ok(data
        .rows()
        .iter()
        .filter_map(|row| {
            Some((
                as_str(row.get(n)?)?.to_string(),
                as_str(row.get(s)?)?.to_string(),
                as_f64(row.get(v)?)?,
                as_i64(row.get(t)?)? / 1_000,
            ))
        })
        .collect())
}

/// `(node, online)` for every node that has ever published a status.
pub async fn fetch_status(client: &Client, status_table: &str) -> Result<Vec<(String, bool)>> {
    let data = client.exec(&latest_status_sql(status_table)).await?;
    let (n, o) = (data.require("node")?, data.require("online")?);
    Ok(data
        .rows()
        .iter()
        .filter_map(|row| {
            let online = match row.get(o)? {
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::String(s) => s == "true",
                _ => return None,
            };
            Some((as_str(row.get(n)?)?.to_string(), online))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Micros = rollup::HOUR;
    const DAY: Micros = rollup::DAY;

    fn all_views() -> Vec<String> {
        rollup::TIERS.iter().map(|t| t.view("readings")).collect()
    }

    #[test]
    fn the_bucket_is_the_finest_step_that_stays_under_the_point_budget() {
        // A day across 500 points wants ~173 s; the ladder's answer is 5m.
        assert_eq!(choose_bucket(0, DAY, 500).1, "5m");
        // An hour across 500 points wants 7.2 s -> 10s.
        assert_eq!(choose_bucket(0, HOUR, 500).1, "10s");
        // A year across 500 points wants ~17 h -> 1d.
        assert_eq!(choose_bucket(0, 365 * DAY, 500).1, "1d");
    }

    #[test]
    fn an_absurd_range_still_produces_a_bucket() {
        // Ten years across one point is wider than the ladder's top step.
        assert_eq!(choose_bucket(0, 3650 * DAY, 1).1, "30d");
        // And a zero-width range does not divide by zero.
        assert_eq!(choose_bucket(5, 5, 500).1, "1s");
    }

    #[test]
    fn a_wide_range_routes_to_a_rollup_and_a_deep_zoom_does_not() {
        let views = all_views();
        let (wide, _) = choose_bucket(0, 365 * DAY, 500);
        assert_eq!(
            rollup::pick(wide, &views, "readings").unwrap().suffix,
            "_1d"
        );
        let (deep, _) = choose_bucket(0, HOUR, 500);
        assert!(rollup::pick(deep, &views, "readings").is_none());
    }

    #[test]
    fn a_rollup_read_re_derives_the_mean_from_sum_and_count() {
        let tier = rollup::TIERS.iter().find(|t| t.suffix == "_1h").unwrap();
        let sql = series_sql("readings", Some(tier), "bad", "humidity", 0, 100, "6h");
        assert!(sql.contains("FROM readings_1h"), "{sql}");
        assert!(sql.contains("sum(sv)/sum(n) av"), "{sql}");
        // Not avg(av): that would weight a sparse bucket like a full one.
        assert!(!sql.contains("avg(av)"), "{sql}");
        assert!(sql.contains("min(lo) lo"), "{sql}");
        assert!(sql.contains("SAMPLE BY 6h ALIGN TO CALENDAR"), "{sql}");
    }

    #[test]
    fn a_base_table_read_reduces_the_raw_column() {
        let sql = series_sql("readings", None, "bad", "humidity", 0, 100, "10s");
        assert!(sql.contains("FROM readings "), "{sql}");
        assert!(
            sql.contains("min(value) lo, max(value) hi, avg(value) av"),
            "{sql}"
        );
    }

    #[test]
    fn both_shapes_return_the_same_columns() {
        // The frontend must not be able to tell the two apart.
        let tier = &rollup::TIERS[0];
        let rollup_sql = series_sql("readings", Some(tier), "n", "s", 0, 1, "1m");
        let raw_sql = series_sql("readings", None, "n", "s", 0, 1, "1m");
        let prefix = "SELECT cast(timestamp AS long) t, lo, hi, av FROM (";
        assert!(rollup_sql.starts_with(prefix), "{rollup_sql}");
        assert!(raw_sql.starts_with(prefix), "{raw_sql}");
    }

    #[test]
    fn the_range_is_half_open_and_cast_from_microseconds() {
        let sql = series_sql("readings", None, "n", "s", 10, 20, "1m");
        assert!(sql.contains("timestamp >= cast(10 AS timestamp)"), "{sql}");
        assert!(sql.contains("timestamp < cast(20 AS timestamp)"), "{sql}");
    }

    #[test]
    fn a_window_is_widened_to_whole_buckets() {
        // Half past the hour, asked at an hourly bucket: both edges move out.
        let (from, to) = snap(90 * rollup::MINUTE, 150 * rollup::MINUTE, HOUR);
        assert_eq!(from, HOUR);
        assert_eq!(to, 3 * HOUR);
    }

    #[test]
    fn a_window_already_on_the_boundaries_does_not_grow() {
        let (from, to) = snap(HOUR, 3 * HOUR, HOUR);
        assert_eq!((from, to), (HOUR, 3 * HOUR));
    }

    #[test]
    fn a_window_narrower_than_one_bucket_still_covers_one() {
        // Otherwise the query would have an empty half-open range and the
        // chart would be blank rather than coarse.
        let (from, to) = snap(HOUR + 1, HOUR + 2, HOUR);
        assert_eq!((from, to), (HOUR, 2 * HOUR));
    }

    #[test]
    fn snapping_works_before_1970_too() {
        // `div_euclid` rather than `/`: truncating division rounds *towards*
        // zero, which for a negative epoch would round the window's start
        // forwards and drop a bucket.
        let (from, _) = snap(-90 * rollup::MINUTE, 0, HOUR);
        assert_eq!(from, -2 * HOUR);
    }

    #[test]
    fn identifiers_that_are_not_topic_segments_are_refused() {
        assert!(checked_literal("schlafzimmer").is_ok());
        assert!(checked_literal("scd41_temperature").is_ok());
        assert!(checked_literal("pm2.5").is_ok());
        assert!(checked_literal("").is_err());
        assert!(checked_literal("a'; DROP TABLE readings--").is_err());
        assert!(checked_literal("has space").is_err());
        assert!(checked_literal(&"x".repeat(200)).is_err());
    }

    #[test]
    fn the_channel_list_comes_from_the_coarsest_view_available() {
        assert!(channels_sql("readings", &all_views()).contains("FROM readings_1d"));
        let partial = vec!["readings_1m".to_string()];
        assert!(channels_sql("readings", &partial).contains("FROM readings_1m"));
        // No views at all: the base table still answers, just slowly.
        assert!(channels_sql("readings", &[]).contains("FROM readings"));
    }

    #[test]
    fn the_overview_asks_for_every_channel_at_once() {
        // No node/sensor filter, and both identifiers in the projection: one
        // statement has to answer a screen with thirty-one sparklines on it.
        let tier = &rollup::TIERS[1];
        let sql = overview_sql("readings", Some(tier), 0, 100, "1h");
        assert!(sql.contains("SELECT timestamp, node, sensor,"), "{sql}");
        assert!(!sql.contains("WHERE node ="), "{sql}");
        assert!(sql.contains("sum(sv)/sum(n) av"), "{sql}");
        // The raw shape has to work too, for a window finer than a minute.
        assert!(overview_sql("readings", None, 0, 100, "10s").contains("avg(value) av"));
    }

    #[test]
    fn a_new_channel_is_found_before_the_coarse_view_has_it() {
        // The rollup is the history; this is the "published since yesterday"
        // half, and it reads two daily partitions rather than the whole table.
        let sql = recent_channels_sql("readings");
        assert!(sql.contains("FROM readings "), "{sql}");
        assert!(sql.contains("dateadd('d', -2, now())"), "{sql}");
    }

    #[test]
    fn latest_uses_questdbs_one_row_per_channel_form() {
        let sql = latest_sql("readings");
        assert!(
            sql.contains("LATEST ON timestamp PARTITION BY node, sensor"),
            "{sql}"
        );
        let sql = latest_status_sql("node_status");
        assert!(
            sql.contains("LATEST ON timestamp PARTITION BY node"),
            "{sql}"
        );
    }
}
