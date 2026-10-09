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

/// Make writing the same reading twice a no-op.
///
/// `(timestamp, node, sensor)` is the natural key of a reading: one channel
/// cannot hold two values at the same instant. With deduplication on, a write
/// of a row that is already there replaces it instead of adding a second copy,
/// so the `_1m` view keeps averaging the distinct readings rather than however
/// many times each one happened to arrive.
///
/// Nothing sends a duplicate today: the timestamp is `now_micros()` at the
/// moment of receipt, so two deliveries of one reading are two different rows
/// and this never fires. It is here for the shape that makes it fire -- a
/// reading carrying its own timestamp from the node, which is what would let
/// the retained value on a state topic be stored on reconnect instead of
/// discarded (see `mqtt::handle`). That is only safe if re-reading the same
/// retained value is idempotent, and this is what makes it so.
///
/// An `ALTER` rather than a clause on the `CREATE`, for the same reason
/// `alter_ttl_ddl` is one: the table on the home server already exists, and
/// `IF NOT EXISTS` leaves it exactly as it is. Re-issuing it is not an error --
/// QuestDB treats the key list as an override -- so it can go out on every
/// start like the TTL does.
pub fn alter_dedup_ddl(table: &str) -> String {
    format!("ALTER TABLE '{table}' DEDUP ENABLE UPSERT KEYS(timestamp, node, sensor)")
}

/// What `SHOW CREATE` says a table or view looks like now, or `None` when the
/// database will not say (an older QuestDB) -- in which case the caller issues
/// its ALTER as before.
async fn show_create(client: &Client, what: &str, name: &str) -> Option<String> {
    let data = client
        .exec(&format!("SHOW CREATE {what} '{name}'"))
        .await
        .ok()?;
    data.rows()
        .first()?
        .first()
        .and_then(as_str)
        .map(str::to_string)
}

/// Whether a table still needs its deduplication keys set.
///
/// Every ALTER is a structure change in the table's WAL, even one that changes
/// nothing, and after the power loss on 2026-10-07 the base table could no
/// longer apply structure changes: each start of this service, by re-issuing
/// these, suspended ingestion until someone skipped the transaction (#53). So
/// they go out only when the table does not already say what they would set.
pub fn needs_dedup(current_ddl: &str) -> bool {
    !current_ddl
        .replace(' ', "")
        .contains("DEDUPUPSERTKEYS(timestamp,node,sensor)")
}

/// Whether a table or view still needs `retention` set as its TTL.
pub fn needs_ttl(current_ddl: &str, retention: &Retention) -> bool {
    match retention.as_sql() {
        Some(ttl) => !current_ddl.contains(&format!("TTL {ttl}")),
        None => false,
    }
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
    annotations_table: &str,
    retention: &Retention,
    rollup_retention: &Retention,
    rollups: bool,
) -> Result<Vec<String>> {
    client.exec(&create_table_ddl(table, retention)).await?;
    client
        .exec(&create_status_ddl(status_table, retention))
        .await?;
    // No retention argument, and that is the whole point of it being a separate
    // statement: a note has to outlive the readings it explains.
    client
        .exec(&super::annotations::create_table_ddl(annotations_table))
        .await?;
    // And bring an older one up to the current shape. `IF NOT EXISTS` on the
    // CREATE above means an existing table is left untouched, so a database
    // written before notes could be taken back would never grow the column.
    client
        .exec(&super::annotations::add_voided_ddl(annotations_table))
        .await?;

    // Not fatal either: deduplication needs a WAL table, and a database that
    // refuses it should keep ingesting rather than fail to start. Losing it
    // costs nothing until a reading brings its own timestamp -- see
    // `alter_dedup_ddl` -- but it is the one thing that has to be in place
    // *before* that, so it goes out on every start.
    let table_ddl = show_create(client, "TABLE", table).await;
    if table_ddl.as_deref().is_none_or(needs_dedup) {
        if let Err(e) = client.exec(&alter_dedup_ddl(table)).await {
            warn!(
                table,
                error = %e,
                "could not enable deduplication; a reading ingested twice would be stored twice"
            );
        }
    }

    for t in [table, status_table] {
        let current = show_create(client, "TABLE", t).await;
        if current
            .as_deref()
            .is_some_and(|ddl| !needs_ttl(ddl, retention))
        {
            continue;
        }
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
        let current = show_create(client, "MATERIALIZED VIEW", &tier.view(table)).await;
        if current
            .as_deref()
            .is_some_and(|ddl| !needs_ttl(ddl, kept_for))
        {
            continue;
        }
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
    Ok(usable_views(table, &views))
}

/// Every materialized view QuestDB currently holds *and is keeping up to
/// date*, by name.
///
/// Asked of the database rather than assumed from `TIERS`, because "the DDL was
/// issued" and "the view exists" are different claims -- a view can also be
/// dropped by hand, or invalidated. An `invalid` view stops refreshing but
/// still answers queries, with whatever it held when it stopped: after the
/// unclean power loss on 2026-10-07 every chart of a day or more read data
/// that ended there, and nothing said so (#53). So only `valid` views count.
pub async fn list_views(client: &Client) -> Result<Vec<String>> {
    let data = client
        .exec("SELECT view_name, view_status FROM materialized_views()")
        .await?;
    let (name, status) = (data.require("view_name")?, data.require("view_status")?);
    Ok(data
        .rows()
        .iter()
        .filter(|row| row.get(status).and_then(as_str) == Some("valid"))
        .filter_map(|row| row.get(name).and_then(as_str).map(str::to_string))
        .collect())
}

/// The views a read may use: the valid ones whose whole chain back to the base
/// table is valid too.
///
/// A tier reads the next finer one, so a valid `_1h` over an invalid `_1m`
/// reports itself healthy while holding exactly what `_1m` stopped at -- or
/// nothing, once `_1m` has been truncated for a rebuild.
pub fn usable_views(table: &str, valid: &[String]) -> Vec<String> {
    let mut usable: Vec<String> = Vec::new();
    for tier in TIERS {
        let view = tier.view(table);
        let source_ok = match tier.source_suffix {
            None => true,
            Some(_) => usable.contains(&tier.source(table)),
        };
        if source_ok && valid.contains(&view) {
            usable.push(view);
        }
    }
    usable
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SHOW CREATE TABLE readings` on family-server, 2026-10-09.
    const READINGS: &str = "CREATE TABLE 'readings' ( \n\ttimestamp TIMESTAMP,\n\tnode SYMBOL INDEX CAPACITY 256,\n\tsensor SYMBOL,\n\tvalue DOUBLE\n) timestamp(timestamp) PARTITION BY DAY TTL 3 YEARS\nDEDUP UPSERT KEYS(timestamp,node,sensor);";
    const STATUS: &str = "CREATE TABLE 'node_status' ( \n\ttimestamp TIMESTAMP,\n\tnode SYMBOL INDEX CAPACITY 256,\n\tonline BOOLEAN\n) timestamp(timestamp) PARTITION BY MONTH TTL 3 YEARS;";
    const VIEW_1H: &str = "CREATE MATERIALIZED VIEW 'readings_1h' WITH BASE 'readings_1m' REFRESH IMMEDIATE AS (\nSELECT timestamp, node, sensor, min(lo) lo, max(hi) hi, sum(sv) sv, sum(n) n FROM readings_1m SAMPLE BY 1h\n) PARTITION BY MONTH TTL 50 YEARS;";

    #[test]
    fn an_unchanged_table_gets_no_alter_on_start() {
        // The whole point: a restart against the live database sends nothing,
        // so it cannot suspend a table that chokes on structure changes.
        assert!(!needs_dedup(READINGS));
        assert!(!needs_ttl(READINGS, &ttl()));
        assert!(!needs_ttl(STATUS, &ttl()));
        assert!(!needs_ttl(VIEW_1H, &Retention::parse("50y").unwrap()));
    }

    #[test]
    fn a_changed_setting_still_goes_out() {
        assert!(needs_dedup(STATUS));
        assert!(needs_ttl(READINGS, &Retention::parse("5y").unwrap()));
        assert!(needs_ttl(VIEW_1H, &ttl()));
    }

    #[test]
    fn a_view_over_a_broken_view_is_not_usable() {
        let all = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // What family-server showed after the 2026-10-07 power loss and a
        // rebuild attempt: the coarse views valid, the minute view not.
        assert!(usable_views("readings", &all(&["readings_1h", "readings_1d"])).is_empty());
        assert_eq!(
            usable_views("readings", &all(&["readings_1m", "readings_1h"])),
            all(&["readings_1m", "readings_1h"])
        );
        assert_eq!(
            usable_views(
                "readings",
                &all(&["readings_1m", "readings_1h", "readings_1d"])
            )
            .len(),
            3
        );
    }

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
        assert_eq!(
            ddl,
            "ALTER MATERIALIZED VIEW 'readings_1d' SET TTL 50 YEARS"
        );
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
    fn a_reading_is_keyed_by_its_channel_and_its_instant() {
        assert_eq!(
            alter_dedup_ddl("readings"),
            "ALTER TABLE 'readings' DEDUP ENABLE UPSERT KEYS(timestamp, node, sensor)"
        );
    }

    #[test]
    fn the_dedup_key_is_the_designated_timestamp_plus_real_columns() {
        // QuestDB refuses an UPSERT KEYS list that omits the designated
        // timestamp, and a key of (node, sensor) alone would collapse each
        // channel's whole history into a single row -- every reading upserting
        // over the last one. Naming a column the table does not have is the
        // other way to get this wrong, so both are checked against the DDL that
        // creates it rather than against a literal.
        let ddl = alter_dedup_ddl("readings");
        let keys = ddl
            .split("UPSERT KEYS(")
            .nth(1)
            .unwrap()
            .trim_end_matches(')');
        assert!(keys.split(", ").any(|k| k == "timestamp"), "{ddl}");

        let create = create_table_ddl("readings", &ttl());
        for key in keys.split(", ") {
            assert!(create.contains(key), "{key} is not a column: {create}");
        }
    }

    #[test]
    fn the_status_table_is_its_own_shape() {
        let ddl = create_status_ddl("node_status", &ttl());
        assert!(ddl.contains("online BOOLEAN"), "{ddl}");
        assert!(ddl.contains("PARTITION BY MONTH"), "{ddl}");
    }
}
