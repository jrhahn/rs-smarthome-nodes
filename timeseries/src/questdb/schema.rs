//! Everything that creates or alters a table, and nothing that reads one.
//!
//! The service owns its schema the way the gateway firmware does: the DDL runs
//! at start-up, every statement is `IF NOT EXISTS`, and the TTL is re-applied
//! on every start so changing `retention` in the settings file is enough to
//! change the retention of the table that already exists. There is no migration
//! step to remember and nothing to run by hand on the home server.
//!
//! View creation is deliberately *best-effort*: a QuestDB too old for
//! materialized views (they need 9.x; table TTL needs 8.2.2) logs a warning and
//! carries on, and the reader then answers every chart from the base table.
//! Slower, identical numbers -- which is the right way round for a service
//! whose job is not to lose data.

use anyhow::Result;
use tracing::{info, warn};

use super::client::{as_str, Client};
use super::rollup::{Keeps, Tier, TIERS};
use crate::config::Retention;

/// The base table: one row per reading.
///
/// `node` is indexed and `sensor` is not, on purpose. Every query in the
/// dashboard filters on both, but an index only earns its keep where it cuts
/// the scan down a lot; with a handful of nodes and a dozen channels each, the
/// symbol column alone is already a cheap integer comparison.
pub fn create_table_ddl(table: &str, retention: &Retention) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS '{table}' (\
         timestamp TIMESTAMP, \
         node SYMBOL CAPACITY 64 INDEX, \
         sensor SYMBOL CAPACITY 512, \
         value DOUBLE\
         ) TIMESTAMP(timestamp) PARTITION BY DAY{ttl}",
        ttl = retention.clause()
    )
}

/// Availability transitions. Partitioned by month because there are only a
/// handful of rows a day even when a node is flapping.
pub fn create_status_ddl(table: &str, retention: &Retention) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS '{table}' (\
         timestamp TIMESTAMP, \
         node SYMBOL CAPACITY 64 INDEX, \
         online BOOLEAN\
         ) TIMESTAMP(timestamp) PARTITION BY MONTH{ttl}",
        ttl = retention.clause()
    )
}

/// Re-apply the retention to a table that already exists.
pub fn alter_ttl_ddl(table: &str, retention: &Retention) -> Option<String> {
    retention
        .as_sql()
        .map(|ttl| format!("ALTER TABLE '{table}' SET TTL {ttl}"))
}

/// One rollup view.
///
/// The two shapes differ only in the columns they read: the tier that reads the
/// raw table reduces `value`, every coarser tier re-reduces the finer view's
/// own `lo`/`hi`/`sv`/`n`. Keeping the output columns identical across tiers is
/// what makes the cascade possible at all -- and what lets the reader use one
/// query for every tier.
pub fn create_view_ddl(base: &str, tier: &Tier, retention: &Retention) -> String {
    // `retention` here is already the one this tier is kept for; the caller
    // picks between the two settings (see `for_tier`).
    let aggregates = if tier.reads_raw() {
        "min(value) lo, max(value) hi, sum(value) sv, count() n"
    } else {
        "min(lo) lo, max(hi) hi, sum(sv) sv, sum(n) n"
    };
    format!(
        "CREATE MATERIALIZED VIEW IF NOT EXISTS '{view}' REFRESH IMMEDIATE AS (\
         SELECT timestamp, node, sensor, {aggregates} \
         FROM {source} SAMPLE BY {bucket}\
         ) PARTITION BY {partition}{ttl}",
        view = tier.view(base),
        source = tier.source(base),
        bucket = tier.bucket,
        partition = tier.partition,
        ttl = retention.clause()
    )
}

/// Which of the two retentions a tier is kept for.
pub fn for_tier<'a>(tier: &Tier, raw: &'a Retention, rollup: &'a Retention) -> &'a Retention {
    match tier.keeps {
        Keeps::WithTheRawTable => raw,
        Keeps::LongTerm => rollup,
    }
}

/// Change a materialized view's TTL.
///
/// Views need their own statement: `ALTER TABLE ... SET TTL` is refused with
/// "cannot modify materialized view", and the working form cannot clear a TTL
/// either -- zero comes back as "TTL value must be an integer multiple of
/// partition size". A view created with a TTL therefore has one for ever, which
/// is why the rollups are kept for a long time rather than for an unlimited one.
pub fn alter_view_ttl_ddl(view: &str, retention: &Retention) -> Option<String> {
    retention
        .as_sql()
        .map(|ttl| format!("ALTER MATERIALIZED VIEW '{view}' SET TTL {ttl}"))
}

/// Create what is missing and re-apply the retention. Returns the views that
/// exist afterwards, which is what the reader routes against.
pub async fn ensure(
    client: &Client,
    table: &str,
    status_table: &str,
    retention: &Retention,
    rollup_retention: &Retention,
    rollups: bool,
) -> Result<Vec<String>> {
    client.exec(&create_table_ddl(table, retention)).await?;
    client
        .exec(&create_status_ddl(status_table, retention))
        .await?;

    for t in [table, status_table] {
        if let Some(ddl) = alter_ttl_ddl(t, retention) {
            // Not fatal: on a QuestDB without table TTL the data simply keeps
            // accumulating, which is a disk-space problem and not a data-loss
            // one. Say so loudly and keep ingesting.
            if let Err(e) = client.exec(&ddl).await {
                warn!(table = t, error = %e, "could not set the table TTL; data will not expire");
            }
        }
    }
    if retention.is_set() {
        info!(retention = retention.as_sql(), "retention applied");
    } else {
        info!("no retention configured; data is kept indefinitely");
    }

    if !rollups {
        info!("rollup views disabled; every chart will read the base table");
        return Ok(Vec::new());
    }

    for tier in TIERS {
        let kept_for = for_tier(tier, retention, rollup_retention);
        let ddl = create_view_ddl(table, tier, kept_for);
        match client.exec(&ddl).await {
            Ok(_) => info!(
                view = tier.view(table),
                retention = kept_for.as_sql().unwrap_or("unlimited"),
                "rollup view ready"
            ),
            Err(e) => warn!(
                view = tier.view(table),
                error = %e,
                "could not create the rollup view; charts at this grain will read the base table"
            ),
        }

        // Re-applied for the same reason the tables' TTL is: `IF NOT EXISTS`
        // leaves an existing view alone, so a changed setting would otherwise
        // only ever reach a database that did not have the view yet -- which is
        // every database except the one that matters.
        if let Some(ddl) = alter_view_ttl_ddl(&tier.view(table), kept_for) {
            if let Err(e) = client.exec(&ddl).await {
                warn!(view = tier.view(table), error = %e, "could not set the view's TTL");
            }
        }
    }

    let views = list_views(client).await.unwrap_or_else(|e| {
        warn!(error = %e, "could not list materialized views; assuming none");
        Vec::new()
    });
    Ok(views)
}

/// Every materialized view QuestDB currently holds, by name.
///
/// Asked of the database rather than assumed from `TIERS`, because "the DDL was
/// issued" and "the view exists" are different claims -- a view can also be
/// dropped by hand, or invalidated.
pub async fn list_views(client: &Client) -> Result<Vec<String>> {
    let data = client
        .exec("SELECT view_name FROM materialized_views()")
        .await?;
    let idx = data.require("view_name")?;
    Ok(data
        .rows()
        .iter()
        .filter_map(|row| row.get(idx).and_then(as_str).map(str::to_string))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ttl() -> Retention {
        Retention::parse("3y").unwrap()
    }

    #[test]
    fn the_base_table_is_partitioned_and_has_the_ttl() {
        let ddl = create_table_ddl("readings", &ttl());
        assert!(
            ddl.contains("CREATE TABLE IF NOT EXISTS 'readings'"),
            "{ddl}"
        );
        assert!(ddl.contains("TIMESTAMP(timestamp)"), "{ddl}");
        assert!(ddl.contains("PARTITION BY DAY"), "{ddl}");
        assert!(ddl.ends_with("TTL 3 YEARS"), "{ddl}");
        // The ILP writer sends exactly these names; a rename here without one
        // there would create a second, half-empty set of columns.
        assert!(ddl.contains("node SYMBOL"), "{ddl}");
        assert!(ddl.contains("sensor SYMBOL"), "{ddl}");
        assert!(ddl.contains("value DOUBLE"), "{ddl}");
    }

    #[test]
    fn the_summaries_are_kept_longer_than_the_readings() {
        let raw = Retention::parse("3y").unwrap();
        let long = Retention::parse("50y").unwrap();
        let by = |suffix: &str| TIERS.iter().find(|t| t.suffix == suffix).unwrap();

        assert_eq!(for_tier(by("_1m"), &raw, &long).as_sql(), Some("3 YEARS"));
        for suffix in ["_1h", "_1d"] {
            assert_eq!(
                for_tier(by(suffix), &raw, &long).as_sql(),
                Some("50 YEARS"),
                "{suffix}"
            );
        }
    }

    #[test]
    fn a_views_ttl_is_altered_with_its_own_statement() {
        // `ALTER TABLE` is refused on a materialized view, so the table form
        // must not be what reaches one.
        let ddl = alter_view_ttl_ddl("readings_1d", &Retention::parse("50y").unwrap()).unwrap();
        assert_eq!(ddl, "ALTER MATERIALIZED VIEW 'readings_1d' SET TTL 50 YEARS");
        assert!(!ddl.starts_with("ALTER TABLE"));
    }

    #[test]
    fn without_retention_no_ttl_clause_is_emitted() {
        let none = Retention::parse("").unwrap();
        assert!(!create_table_ddl("readings", &none).contains("TTL"));
        assert!(!create_view_ddl("readings", &TIERS[0], &none).contains("TTL"));
        assert_eq!(alter_ttl_ddl("readings", &none), None);
    }

    #[test]
    fn the_ttl_is_re_applied_to_an_existing_table() {
        assert_eq!(
            alter_ttl_ddl("readings", &ttl()).unwrap(),
            "ALTER TABLE 'readings' SET TTL 3 YEARS"
        );
    }

    #[test]
    fn the_finest_view_reduces_the_raw_column() {
        let ddl = create_view_ddl("readings", &TIERS[0], &ttl());
        assert!(ddl.contains("'readings_1m'"), "{ddl}");
        assert!(ddl.contains("REFRESH IMMEDIATE"), "{ddl}");
        assert!(ddl.contains("FROM readings SAMPLE BY 1m"), "{ddl}");
        assert!(ddl.contains("min(value) lo"), "{ddl}");
        assert!(ddl.contains("sum(value) sv"), "{ddl}");
        assert!(ddl.contains("count() n"), "{ddl}");
        assert!(ddl.contains("PARTITION BY DAY TTL 3 YEARS"), "{ddl}");
    }

    #[test]
    fn a_coarse_view_reads_the_next_finer_view() {
        let hour = TIERS.iter().find(|t| t.suffix == "_1h").unwrap();
        let ddl = create_view_ddl("readings", hour, &ttl());
        assert!(ddl.contains("FROM readings_1m SAMPLE BY 1h"), "{ddl}");
        // Re-reduction, not re-aggregation of the raw column: `value` does not
        // exist in the source view at all.
        assert!(ddl.contains("min(lo) lo"), "{ddl}");
        assert!(ddl.contains("sum(sv) sv"), "{ddl}");
        assert!(ddl.contains("sum(n) n"), "{ddl}");
        assert!(!ddl.contains("value"), "{ddl}");
    }

    #[test]
    fn no_tier_is_left_on_a_timer() {
        // See the note on TIERS: a timer-refreshed view created before its
        // source has any rows was observed not to catch up at all, and at this
        // fleet's ingest rate the timer saves nothing anyway.
        for tier in TIERS {
            let ddl = create_view_ddl("readings", tier, &ttl());
            assert!(ddl.contains("REFRESH IMMEDIATE"), "{ddl}");
            assert!(!ddl.contains("REFRESH EVERY"), "{ddl}");
        }
    }

    #[test]
    fn every_tier_keeps_the_same_output_columns() {
        // The cascade only works while a coarser tier can consume a finer
        // one's output, and the reader relies on one query shape for all of
        // them.
        for tier in TIERS {
            let ddl = create_view_ddl("readings", tier, &ttl());
            for col in [" lo,", " hi,", " sv,", " n "] {
                assert!(ddl.contains(col), "{} lacks {col}: {ddl}", tier.suffix);
            }
            assert!(ddl.contains("SELECT timestamp, node, sensor,"), "{ddl}");
        }
    }

    #[test]
    fn a_views_partition_is_never_finer_than_its_bucket() {
        // QuestDB rejects that outright, and a daily rollup partitioned by day
        // would also put a single row per channel in each partition.
        for tier in TIERS {
            let rank = |unit: &str| match unit {
                "HOUR" => 0,
                "DAY" => 1,
                "WEEK" => 2,
                "MONTH" => 3,
                "YEAR" => 4,
                other => panic!("unknown partition unit {other}"),
            };
            let bucket_rank = match tier.bucket {
                "1m" => 0,
                "1h" => 0,
                "1d" => 1,
                other => panic!("unknown bucket {other}"),
            };
            assert!(rank(tier.partition) >= bucket_rank, "{}", tier.suffix);
        }
    }

    #[test]
    fn the_status_table_is_its_own_shape() {
        let ddl = create_status_ddl("node_status", &ttl());
        assert!(ddl.contains("online BOOLEAN"), "{ddl}");
        assert!(ddl.contains("PARTITION BY MONTH"), "{ddl}");
    }
}
