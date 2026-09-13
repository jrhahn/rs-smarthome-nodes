//! The rollup tiers, in one place, because two sides of the service have to
//! agree on them exactly: the writer creates the materialized views, the reader
//! picks one to answer a chart from. A table that drifts between them is a
//! dashboard that silently reads a view the writer never made.
//!
//! # Why views at all, on a fleet this small
//!
//! Not for speed today. Five nodes publishing a reading a minute is ~50 rows a
//! minute -- a full scan of a month of that is nothing. It is for the shape of
//! the question three years in: `retention = 3y` means the base table ends up
//! with on the order of 70 million rows, and "show me the bedroom's CO2 for the
//! last year" then reads all of it to draw 400 pixels. The gateway firmware hit
//! exactly that wall at a much larger scale (~24.9 s for a 7-day chart over its
//! `sensors` table) and the answer there is the answer here.
//!
//! # Why they cascade
//!
//! Each tier aggregates the *next finer view*, not the base table. QuestDB
//! refreshes a view by re-aggregating the buckets the new rows touched, so a
//! `_1d` view built on the raw table re-scans the whole current day on every
//! commit -- once per flush interval, for ever. Built on `_1h` it re-reads at
//! most 24 rows per channel instead. On the gateway that difference was ~3 CPU
//! cores against a few percent.
//!
//! # Why sum + count rather than avg
//!
//! QuestDB allows one aggregate per output column, so a view cannot store an
//! average *and* the count needed to re-aggregate it correctly. Storing
//! `sum` and `count` instead lets the reader derive the exact mean at any
//! coarser bucket as `sum(sv)/sum(n)`, rather than an average of averages that
//! quietly weights a sparse hour the same as a full one. `min`/`max` come along
//! so the envelope keeps its peaks at every zoom level.

use crate::model::Micros;

pub const MINUTE: Micros = 60 * 1_000_000;
pub const HOUR: Micros = 60 * MINUTE;
pub const DAY: Micros = 24 * HOUR;

/// One rollup level.
#[derive(Debug, Clone, Copy)]
pub struct Tier {
    /// Appended to the base table name: `readings` + `_1m`.
    pub suffix: &'static str,
    /// The `SAMPLE BY` bucket.
    pub bucket: &'static str,
    /// Bucket width, for routing a read to the coarsest tier that still has
    /// enough resolution.
    pub grain: Micros,
    /// Which table this tier aggregates: `None` is the base table, `Some` is a
    /// finer tier's suffix.
    pub source_suffix: Option<&'static str>,
    /// Partition unit for the view. Never finer than the bucket, or QuestDB
    /// rejects it; never so coarse that the TTL cannot drop anything.
    pub partition: &'static str,
    /// Which of the two retentions this tier is kept for.
    pub keeps: Keeps,
}

/// How long a tier is kept, as a choice between the two settings rather than a
/// number: the tiers differ in kind, not in how many years someone picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keeps {
    /// As long as the raw table. The minute view is the same data at the same
    /// order of magnitude -- about as many rows as the readings themselves --
    /// so keeping it longer than what it summarises buys nothing.
    WithTheRawTable,
    /// Far longer than the readings. The rollups were built for *speed*, never
    /// to save space: a day of `_1d` is thirty-one rows, and it is the only
    /// part of the archive a question spanning years actually reads. Letting
    /// the summaries die with their readings would throw exactly that away to
    /// reclaim nothing. See `docs/long-term-history.md` for the measurement.
    LongTerm,
}

/// Finest first. Ordering is load-bearing: `source_suffix` refers backwards
/// into this list, and creation walks it forwards so a view's source exists
/// before the view does.
///
/// # Why every tier refreshes immediately
///
/// The gateway firmware puts its two coarse tiers on a timer (`REFRESH EVERY
/// 5m` / `1h`) because at 100 Hz ingest an immediate refresh of every tier on
/// every commit is what pegged three CPU cores. This fleet publishes about one
/// reading a minute per channel -- six orders of magnitude less -- and the
/// cascade already bounds each refresh to the parent buckets that moved, so
/// there is nothing left for a timer to save.
///
/// There is also a reason not to use one. On QuestDB 9.3.5 a timer-refreshed
/// view created over a source that was still empty did not pick up 2160 rows
/// inserted into that source over the following twenty minutes
/// (`refresh_base_table_txn` stayed at 1 while the source reached 7); a single
/// manual `REFRESH MATERIALIZED VIEW ... INCREMENTAL` fixed it and it tracked
/// correctly from then on. That is exactly the shape of a first install --
/// views created before the first reading arrives -- so the failure would land
/// on the one run nobody is watching yet. `REFRESH IMMEDIATE` had no such gap
/// in the same test.
pub const TIERS: &[Tier] = &[
    Tier {
        suffix: "_1m",
        bucket: "1m",
        grain: MINUTE,
        source_suffix: None,
        partition: "DAY",
        keeps: Keeps::WithTheRawTable,
    },
    Tier {
        suffix: "_1h",
        bucket: "1h",
        grain: HOUR,
        source_suffix: Some("_1m"),
        partition: "MONTH",
        keeps: Keeps::LongTerm,
    },
    Tier {
        suffix: "_1d",
        bucket: "1d",
        grain: DAY,
        source_suffix: Some("_1h"),
        // A year per partition: with `PARTITION BY DAY` a daily rollup would
        // put one row per channel in a partition of its own, and QuestDB only
        // ever drops whole partitions.
        partition: "YEAR",
        keeps: Keeps::LongTerm,
    },
];

impl Tier {
    /// The view's table name.
    pub fn view(&self, base: &str) -> String {
        format!("{base}{}", self.suffix)
    }

    /// The table this view reads from.
    pub fn source(&self, base: &str) -> String {
        match self.source_suffix {
            Some(suffix) => format!("{base}{suffix}"),
            None => base.to_string(),
        }
    }

    /// Whether this tier aggregates the raw readings rather than another view.
    /// The two cases differ in the columns they read: `value` once, `lo`/`hi`/
    /// `sv`/`n` from then on.
    pub fn reads_raw(&self) -> bool {
        self.source_suffix.is_none()
    }
}

/// The coarsest tier whose buckets are still no wider than `interval`, out of
/// those that actually exist in the database.
///
/// Returns `None` for "read the base table", which covers two cases that want
/// the same thing: a zoom finer than a minute, and a database where the views
/// could not be created (an older QuestDB, say). The reader's fallback is the
/// raw table, which is always correct and merely slower -- so a missing view
/// degrades the dashboard's speed and never its numbers.
pub fn pick<'a>(interval: Micros, available: &[String], base: &str) -> Option<&'a Tier> {
    TIERS
        .iter()
        .rev()
        .find(|t| t.grain <= interval && available.iter().any(|v| *v == t.view(base)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_views(base: &str) -> Vec<String> {
        TIERS.iter().map(|t| t.view(base)).collect()
    }

    #[test]
    fn only_the_finest_tier_expires_with_the_readings() {
        let short: Vec<&str> = TIERS
            .iter()
            .filter(|t| t.keeps == Keeps::WithTheRawTable)
            .map(|t| t.suffix)
            .collect();
        assert_eq!(short, vec!["_1m"], "the coarse summaries are the long-lived ones");
    }

    #[test]
    fn tiers_run_finest_to_coarsest() {
        for pair in TIERS.windows(2) {
            assert!(pair[0].grain < pair[1].grain, "{:?}", pair);
        }
    }

    #[test]
    fn every_cascaded_tier_names_a_source_that_comes_before_it() {
        for (i, tier) in TIERS.iter().enumerate() {
            let Some(src) = tier.source_suffix else {
                assert_eq!(i, 0, "only the finest tier may read the base table");
                continue;
            };
            let pos = TIERS.iter().position(|t| t.suffix == src);
            assert_eq!(pos, Some(i - 1), "{} reads {src}", tier.suffix);
        }
    }

    #[test]
    fn view_and_source_names_are_built_from_the_base_table() {
        let one_h = TIERS.iter().find(|t| t.suffix == "_1h").unwrap();
        assert_eq!(one_h.view("readings"), "readings_1h");
        assert_eq!(one_h.source("readings"), "readings_1m");
        assert!(!one_h.reads_raw());
        assert!(TIERS[0].reads_raw());
        assert_eq!(TIERS[0].source("readings"), "readings");
    }

    #[test]
    fn routing_takes_the_coarsest_tier_that_still_resolves_the_bucket() {
        let views = all_views("readings");
        assert_eq!(pick(DAY, &views, "readings").unwrap().suffix, "_1d");
        assert_eq!(pick(6 * HOUR, &views, "readings").unwrap().suffix, "_1h");
        assert_eq!(pick(HOUR, &views, "readings").unwrap().suffix, "_1h");
        assert_eq!(pick(5 * MINUTE, &views, "readings").unwrap().suffix, "_1m");
        assert_eq!(pick(MINUTE, &views, "readings").unwrap().suffix, "_1m");
    }

    #[test]
    fn a_zoom_finer_than_the_finest_tier_reads_the_base_table() {
        let views = all_views("readings");
        assert!(pick(MINUTE - 1, &views, "readings").is_none());
        assert!(pick(1_000, &views, "readings").is_none());
    }

    #[test]
    fn only_the_views_that_exist_are_routed_to() {
        // A half-created set: `_1d` missing because its REFRESH EVERY was
        // rejected by an older QuestDB. A day-wide bucket must fall back to
        // `_1h` rather than query a view that is not there.
        let partial = vec!["readings_1m".to_string(), "readings_1h".to_string()];
        assert_eq!(pick(DAY, &partial, "readings").unwrap().suffix, "_1h");
        assert!(pick(DAY, &[], "readings").is_none());
    }

    #[test]
    fn views_of_another_table_are_not_mistaken_for_ours() {
        let other = vec!["something_else_1d".to_string()];
        assert!(pick(DAY, &other, "readings").is_none());
    }
}
