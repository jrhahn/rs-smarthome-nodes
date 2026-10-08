//! Values per calendar period: what a day, a week or a month came to.
//!
//! The rest of the dashboard draws a window that slides with the clock. This is
//! the other question, and the one the meters were fitted for: how much water
//! did *Tuesday* take, is this month's electricity more than last month's. That
//! wants buckets that start where a person's day starts -- local midnight,
//! Monday, the 1st -- not where the epoch puts them.
//!
//! Two shapes of channel, and they want different numbers:
//!
//! - **A meter** (`state_class` `total_increasing` / `total`) publishes a
//!   running total. Its period is the *difference* of the total across it: the
//!   last reading of the period minus the last reading of the one before. A
//!   mean of a meter reading is a number nobody can use.
//! - **A measurement** keeps the min / mean / max every other view shows,
//!   just bucketed by the calendar.
//!
//! **Last reading, not the maximum.** A meter only climbs in theory. The water
//! meters are read by a camera, and a misread digit sends one reading up and
//! the next ones back: on 2026-10-06 the cold meter peaked at 8.996 m³ and
//! closed the day at 8.904. The maximum would book that misread as consumption
//! on one day and as a reset on the next. Differences of last readings instead
//! *telescope*: summed over any run of periods they come to the last reading
//! minus the first, whatever happened in between -- a misread shows up once as
//! a negative period and cancels itself.
//!
//! That is also why the two shapes are read from different tables. `last()` is
//! not something the rollup views keep, so meters are read from the base table,
//! filtered to the handful of meter channels. Measurements read `readings_1h`:
//! hourly buckets line up with local days in both CET and CEST -- the offset is
//! always a whole number of hours -- so that is exact, and it is what keeps a
//! year of months cheap. The base table answers when the view is missing or
//! empty, as everywhere else.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::client::{as_f64, as_i64, as_str, Client};
use super::rollup::{self, Tier, DAY, HOUR};
use super::series::checked_literal;
use crate::model::Micros;

/// Where a day starts. The house's, not the server's: the server runs UTC.
pub const TIME_ZONE: &str = "Europe/Berlin";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Day,
    Week,
    Month,
}

impl Period {
    /// The `SAMPLE BY` unit. QuestDB aligns `1w` to Monday and `1M` to the 1st
    /// once it is told the time zone.
    pub fn sample(self) -> &'static str {
        match self {
            Period::Day => "1d",
            Period::Week => "1w",
            Period::Month => "1M",
        }
    }

    /// The longest this period can be, so a window of `n` of them is never
    /// short: a day is 25 hours once a year, a month at most 31 days.
    pub fn longest(self) -> Micros {
        match self {
            Period::Day => DAY + HOUR,
            Period::Week => 7 * DAY + HOUR,
            Period::Month => 31 * DAY + HOUR,
        }
    }

    /// How many to show when the page does not say.
    pub fn default_count(self) -> usize {
        match self {
            Period::Day => 14,
            Period::Week => 12,
            Period::Month => 12,
        }
    }
}

/// Is this channel a running total, by what its discovery message says?
pub fn is_meter(state_class: &str) -> bool {
    matches!(state_class, "total_increasing" | "total")
}

/// One statement for every channel's periods, like the overview's sparklines.
pub fn periods_sql(
    base: &str,
    tier: Option<&Tier>,
    from: Micros,
    to: Micros,
    period: Period,
) -> String {
    let (table, aggregates) = match tier {
        Some(t) => (t.view(base), "min(lo) lo, max(hi) hi, sum(sv)/sum(n) av"),
        None => (
            base.to_string(),
            "min(value) lo, max(value) hi, avg(value) av",
        ),
    };
    let sample = period.sample();
    format!(
        "SELECT cast(timestamp AS long) t, node, sensor, lo, hi, av FROM (\
         SELECT timestamp, node, sensor, {aggregates} FROM {table} \
         WHERE timestamp >= cast({from} AS timestamp) AND timestamp < cast({to} AS timestamp) \
         SAMPLE BY {sample} ALIGN TO CALENDAR TIME ZONE '{TIME_ZONE}')"
    )
}

/// The meters' periods, from the base table, with each period's last reading.
///
/// `nodes` and `sensors` narrow the scan; the pairs they also let through (a
/// meter's node with another meter's key) are dropped by the caller. Both must
/// already have passed `series::checked_literal`.
pub fn meters_sql(
    base: &str,
    nodes: &[&str],
    sensors: &[&str],
    from: Micros,
    to: Micros,
    period: Period,
) -> String {
    let list = |xs: &[&str]| {
        xs.iter()
            .map(|x| format!("'{x}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let sample = period.sample();
    format!(
        "SELECT cast(timestamp AS long) t, node, sensor, lo, hi, av, lv FROM (\
         SELECT timestamp, node, sensor, min(value) lo, max(value) hi, avg(value) av, \
         last(value) lv FROM {base} \
         WHERE node IN ({}) AND sensor IN ({}) \
         AND timestamp >= cast({from} AS timestamp) AND timestamp < cast({to} AS timestamp) \
         SAMPLE BY {sample} ALIGN TO CALENDAR TIME ZONE '{TIME_ZONE}')",
        list(nodes),
        list(sensors)
    )
}

/// One period of one channel.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Bucket {
    /// Period start in milliseconds: local midnight, as an instant.
    pub t: i64,
    pub lo: f64,
    pub hi: f64,
    pub av: f64,
    /// A meter's consumption in this period. `None` for a measurement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<f64>,
    /// The consumption is not a plain difference: there was no period before
    /// it to subtract (the start of the history), or the meter read lower than
    /// at the end of the period before (a misread, or a reset).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub approx: bool,
}

/// A raw row: period start (ms), min, max, mean, and for a meter the period's
/// last reading.
pub type Row = (i64, f64, f64, f64, Option<f64>);

/// The last `keep` periods of one channel, with consumption for a meter.
///
/// `rows` is oldest first and should hold one period more than `keep`, so the
/// first period shown has a predecessor to subtract. A meter's consumption is
/// its last reading minus the previous period's last reading. Without a
/// predecessor it is the period's own climb, from its lowest reading -- a lower
/// bound. A negative difference is kept, not hidden: it is a misread or a
/// reset, and dropping it would break the sum (see the module note).
pub fn buckets(rows: &[Row], keep: usize) -> Vec<Bucket> {
    let start = rows.len().saturating_sub(keep);
    rows.iter()
        .enumerate()
        .skip(start)
        .map(|(i, &(t, lo, hi, av, last))| {
            let previous = i.checked_sub(1).and_then(|p| rows[p].4);
            let (delta, approx) = match (last, previous) {
                (None, _) => (None, false),
                (Some(end), Some(before)) => (Some(end - before), end < before),
                (Some(end), None) => (Some(end - lo), true),
            };
            Bucket {
                t,
                lo,
                hi,
                av,
                delta,
                approx,
            }
        })
        .collect()
}

/// `(node, sensor)` to its rows, oldest first, for every channel with data.
pub type ChannelRows = ((String, String), Vec<Row>);

/// Every channel's periods: measurements from the hourly view, the listed
/// meters from the base table. Returns the table the measurements came from.
pub async fn fetch_rows(
    client: &Client,
    base: &str,
    views: &[String],
    meters: &[(String, String)],
    period: Period,
    count: usize,
    now: Micros,
) -> Result<(String, Vec<ChannelRows>)> {
    // One period more than shown, for the first difference, and one more again
    // because `from` lands inside a period and the first bucket is partial.
    let from = now - (count as i64 + 2) * period.longest();
    let hourly = rollup::TIERS
        .iter()
        .find(|t| t.grain == HOUR && views.iter().any(|v| *v == t.view(base)));

    let mut source = hourly
        .map(|t| t.view(base))
        .unwrap_or_else(|| base.to_string());
    let mut out = run(client, &periods_sql(base, hourly, from, now, period)).await?;
    if hourly.is_some() && out.is_empty() {
        source = base.to_string();
        out = run(client, &periods_sql(base, None, from, now, period)).await?;
    }
    out.retain(|(key, _)| !meters.contains(key));

    let mut nodes: Vec<&str> = Vec::new();
    let mut sensors: Vec<&str> = Vec::new();
    for (node, sensor) in meters {
        let (node, sensor) = (checked_literal(node)?, checked_literal(sensor)?);
        if !nodes.contains(&node) {
            nodes.push(node);
        }
        if !sensors.contains(&sensor) {
            sensors.push(sensor);
        }
    }
    if !nodes.is_empty() {
        let sql = meters_sql(base, &nodes, &sensors, from, now, period);
        let mut found = run(client, &sql).await?;
        found.retain(|(key, _)| meters.contains(key));
        out.extend(found);
    }
    Ok((source, out))
}

async fn run(client: &Client, sql: &str) -> Result<Vec<ChannelRows>> {
    let data = client.exec(sql).await?;
    let (ti, n, s, lo, hi, av) = (
        data.require("t")?,
        data.require("node")?,
        data.require("sensor")?,
        data.require("lo")?,
        data.require("hi")?,
        data.require("av")?,
    );
    let lv = data.column_index("lv");
    let mut out: Vec<ChannelRows> = Vec::new();
    for row in data.rows() {
        let parsed = (|| {
            let key = (
                as_str(row.get(n)?)?.to_string(),
                as_str(row.get(s)?)?.to_string(),
            );
            let last = lv.and_then(|i| row.get(i)).and_then(as_f64);
            let r: Row = (
                as_i64(row.get(ti)?)? / 1_000,
                as_f64(row.get(lo)?)?,
                as_f64(row.get(hi)?)?,
                as_f64(row.get(av)?)?,
                last,
            );
            Some((key, r))
        })();
        let Some((key, r)) = parsed else { continue };
        match out.iter_mut().find(|(k, _)| *k == key) {
            Some((_, rows)) => rows.push(r),
            None => out.push((key, vec![r])),
        }
    }
    // `SAMPLE BY` returns time order already; sorting costs nothing and makes
    // the predecessor rule in `buckets` not depend on it.
    for (_, rows) in &mut out {
        rows.sort_by_key(|r| r.0);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_meter_period_is_the_difference_of_its_last_readings() {
        // Cold water, from the archive: the day closed at 8.904 although it had
        // read 8.996 at noon -- a misread, which the maximum would have booked.
        let rows = [
            (0, 8.939, 8.964, 8.95, Some(8.950)),
            (1, 8.897, 8.996, 8.95, Some(8.904)),
            (2, 8.904, 8.967, 8.92, Some(8.967)),
        ];
        let b = buckets(&rows, 2);
        assert_eq!(b.len(), 2);
        assert!((b[0].delta.unwrap() + 0.046).abs() < 1e-9);
        assert!(b[0].approx, "a meter that went backwards is flagged");
        assert!((b[1].delta.unwrap() - 0.063).abs() < 1e-9);
        assert!(!b[1].approx);
    }

    #[test]
    fn the_periods_add_up_to_the_meter_whatever_happened_between() {
        let rows = [
            (0, 0.0, 1.0, 0.5, Some(1.0)),
            (1, 1.0, 9.0, 3.0, Some(2.0)), // a spike to 9 inside the period
            (2, 0.5, 2.5, 1.0, Some(0.5)), // a misread at the end
            (3, 0.5, 3.0, 2.0, Some(3.0)),
        ];
        let total: f64 = buckets(&rows, 3).iter().map(|b| b.delta.unwrap()).sum();
        assert!((total - (3.0 - 1.0)).abs() < 1e-9);
    }

    #[test]
    fn the_first_period_of_a_history_is_marked_as_a_lower_bound() {
        let b = buckets(&[(0, 1.0, 1.5, 1.2, Some(1.5))], 5);
        assert_eq!(b[0].delta, Some(0.5));
        assert!(b[0].approx);
    }

    #[test]
    fn a_measurement_has_no_consumption() {
        let b = buckets(&[(0, 18.0, 24.0, 21.0, None)], 1);
        assert_eq!(b[0].delta, None);
        assert!(!b[0].approx);
    }

    #[test]
    fn meters_are_read_by_their_last_reading_from_the_base_table() {
        let sql = meters_sql(
            "readings",
            &["wasserzaehler_kalt"],
            &["value"],
            0,
            100,
            Period::Day,
        );
        assert!(sql.contains("last(value) lv FROM readings "), "{sql}");
        assert!(
            sql.contains("node IN ('wasserzaehler_kalt') AND sensor IN ('value')"),
            "{sql}"
        );
        assert!(
            sql.contains("SAMPLE BY 1d ALIGN TO CALENDAR TIME ZONE 'Europe/Berlin'"),
            "{sql}"
        );
    }

    #[test]
    fn the_calendar_comes_from_the_house_not_the_server() {
        let tier = rollup::TIERS.iter().find(|t| t.grain == HOUR).unwrap();
        let sql = periods_sql("readings", Some(tier), 0, 100, Period::Week);
        assert!(
            sql.contains("SAMPLE BY 1w ALIGN TO CALENDAR TIME ZONE 'Europe/Berlin'"),
            "{sql}"
        );
        assert!(sql.contains("FROM readings_1h "), "{sql}");
        assert!(sql.contains("sum(sv)/sum(n) av"), "{sql}");
        let raw = periods_sql("readings", None, 0, 100, Period::Month);
        assert!(
            raw.contains("FROM readings ") && raw.contains("SAMPLE BY 1M"),
            "{raw}"
        );
    }

    #[test]
    fn only_running_totals_are_meters() {
        assert!(is_meter("total_increasing"));
        assert!(is_meter("total"));
        assert!(!is_meter("measurement"));
        assert!(!is_meter(""));
    }
}
