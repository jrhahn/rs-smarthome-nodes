//! Notes about what happened to the fleet, stored beside the readings.
//!
//! A series outlives the memory of the hardware that produced it. The terrace's
//! weight steps on 2026-09-09 because the scale was recalibrated; the living
//! room's VOC index climbs from 1 for a day because Sensirion's algorithm is
//! learning the room; there is a hole in that channel because a jumper cable
//! was failing. None of that is in the numbers, and in two years none of it
//! will be in anyone's head either.
//!
//! `docs/annotations.md` in this repository keeps the same list by hand and is
//! the better place for the long explanations. What this table adds is *where*:
//! a note with a timestamp can be drawn on the chart it explains, at the point
//! it explains, which is where the question gets asked.
//!
//! Deliberately never expired. A note is a few dozen bytes and it is worth
//! nothing at all if it is dropped before the data it annotates.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::client::{as_i64, as_str, Client};
use super::series::checked_literal;
use crate::model::Micros;

/// Longest note accepted. Long enough for a sentence with a reason in it,
/// short enough that nobody pastes a log into the chart.
pub const NOTE_MAX: usize = 500;

/// A note, as the API hands it out.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Annotation {
    /// When it happened, in milliseconds.
    pub at_ms: i64,
    /// Which node it concerns; empty for something fleet-wide, like the day the
    /// archive itself started.
    #[serde(default)]
    pub node: String,
    pub note: String,
}

/// The table. No TTL clause anywhere, on purpose -- see the module note.
pub fn create_table_ddl(table: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS '{table}' (\
         timestamp TIMESTAMP, \
         node SYMBOL CAPACITY 64, \
         note VARCHAR, \
         voided BOOLEAN\
         ) TIMESTAMP(timestamp) PARTITION BY YEAR"
    )
}

/// Add `voided` to a table written before it existed.
///
/// Separate from the `CREATE`, because `IF NOT EXISTS` on the table means an
/// existing one is left exactly as it was -- including without this column. Run
/// on every start; QuestDB accepts it repeatedly and backfills the existing
/// rows as `false`, which is the right answer for every note written before
/// anyone could take one back.
pub fn add_voided_ddl(table: &str) -> String {
    format!("ALTER TABLE '{table}' ADD COLUMN IF NOT EXISTS voided BOOLEAN")
}

/// Escape a note for an ILP *field* value.
///
/// Different rules from a tag: the value is quoted, so what has to be escaped is
/// the quote and the backslash rather than spaces and commas. Newlines are
/// turned into spaces rather than escaped, for the same reason as in the
/// reading writer -- a raw newline ends the line early and turns the rest into a
/// second, malformed record.
pub fn escape_note(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 8);
    for c in raw.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

/// The node a note is filed under. Empty means the fleet as a whole -- the day
/// the archive started belongs to no single box.
pub fn stored_node(node: &str) -> &str {
    if node.is_empty() {
        "fleet"
    } else {
        node
    }
}

/// One note as a line of ILP.
pub fn ilp_line(table: &str, a: &Annotation) -> String {
    format!(
        "{table},node={node} note=\"{note}\",voided=false {nanos}\n",
        table = super::writer::escape_tag(table),
        node = super::writer::escape_tag(stored_node(&a.node)),
        note = escape_note(&a.note),
        nanos = a.at_ms * 1_000_000,
    )
}

/// Reject what should not be stored, before it reaches the database.
pub fn check(a: &Annotation) -> Result<()> {
    if a.note.trim().is_empty() {
        bail!("a note without text explains nothing");
    }
    if a.note.chars().count() > NOTE_MAX {
        bail!("a note longer than {NOTE_MAX} characters belongs in docs/annotations.md");
    }
    if a.at_ms <= 0 {
        bail!("a note needs a timestamp; 0 would file it under 1970");
    }
    Ok(())
}

/// How long to keep asking whether a write became visible.
///
/// QuestDB answers a write before it has applied it: ILP over HTTP returns 204
/// once the line is accepted, and `UPDATE` returns `{"dml":"OK"}` with an
/// `updated` count that is a running total and not the number of rows this
/// statement touched -- an `UPDATE` matching nothing reports exactly the same
/// shape as one that matched. So neither answer means "stored", and for this
/// table the difference matters: a note is written once, by hand, about
/// something that will not be remembered well enough to write again.
///
/// Two seconds is generous for a WAL that normally applies in milliseconds, and
/// it is spent only on the handful of notes a year that go through here.
const CONFIRM_ATTEMPTS: u32 = 20;
const CONFIRM_DELAY: Duration = Duration::from_millis(100);

/// Count the notes standing at one instant for one node.
///
/// `live` picks between "notes that still count" and "any row at all", which is
/// what tells a note that was never written apart from one already taken back.
pub fn count_sql(table: &str, at_ms: i64, node: &str, live: bool) -> Result<String> {
    let node =
        checked_literal(stored_node(node)).context("a note's node has to look like a node name")?;
    let micros = at_ms * 1_000;
    let voided = if live { " AND NOT voided" } else { "" };
    Ok(format!(
        "SELECT count() c FROM {table} \
         WHERE timestamp = cast({micros} AS timestamp) AND node = '{node}'{voided}"
    ))
}

async fn count(client: &Client, table: &str, at_ms: i64, node: &str, live: bool) -> Result<i64> {
    let data = client.exec(&count_sql(table, at_ms, node, live)?).await?;
    let c = data.require("c")?;
    Ok(data
        .rows()
        .first()
        .and_then(|row| as_i64(row.get(c)?))
        .unwrap_or(0))
}

/// Wait until the table agrees with what we just asked it to do.
async fn settle(
    client: &Client,
    table: &str,
    at_ms: i64,
    node: &str,
    live: bool,
    want: impl Fn(i64) -> bool,
) -> bool {
    for attempt in 0..CONFIRM_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(CONFIRM_DELAY).await;
        }
        if matches!(count(client, table, at_ms, node, live).await, Ok(n) if want(n)) {
            return true;
        }
    }
    false
}

/// Write one note, and do not return until it is actually there.
///
/// Synchronous rather than batched with the readings: there are a handful a
/// year, and whoever wrote it wants to know it landed. The read-back is what
/// makes that true rather than merely intended -- a line accepted by the ILP
/// endpoint against a table that was created moments earlier can be dropped,
/// silently, and the caller would otherwise be told it succeeded.
pub async fn insert(client: &Client, table: &str, a: &Annotation) -> Result<()> {
    check(a)?;
    client.write_ilp(&ilp_line(table, a)).await?;
    if settle(client, table, a.at_ms, &a.node, false, |n| n > 0).await {
        return Ok(());
    }
    bail!(
        "the note was accepted but never appeared in {table}; it is not stored, \
         and writing it again is the right thing to do"
    )
}

/// SQL that takes a note back.
pub fn void_sql(table: &str, at_ms: i64, node: &str) -> Result<String> {
    let node =
        checked_literal(stored_node(node)).context("a note's node has to look like a node name")?;
    let micros = at_ms * 1_000;
    Ok(format!(
        "UPDATE {table} SET voided = true \
         WHERE timestamp = cast({micros} AS timestamp) AND node = '{node}'"
    ))
}

/// Take a note back.
///
/// Not a delete, because QuestDB has none, and not a rewrite, because the
/// timestamp is the designated column and no `UPDATE` may touch it -- which is
/// the trap this exists for. A note filed at the wrong instant cannot be moved;
/// it can only be marked as not counting, and the corrected one written beside
/// it. Without this, one mistyped timestamp costs the whole table.
///
/// Marking rather than removing is also the better record. A log of what
/// happened to the fleet that quietly loses its own corrections is a log with
/// a blind spot exactly where somebody already got something wrong once.
///
/// Addressed by instant and node rather than by an id, so two notes filed at
/// the same millisecond for the same node go together. That is the case worth
/// having: a double-write is the commonest way to end up with one.
pub async fn void(client: &Client, table: &str, at_ms: i64, node: &str) -> Result<()> {
    if count(client, table, at_ms, node, true).await? == 0 {
        bail!("there is no note standing at that instant for that node");
    }
    client.exec(&void_sql(table, at_ms, node)?).await?;
    if settle(client, table, at_ms, node, true, |n| n == 0).await {
        return Ok(());
    }
    bail!("the note is still standing after the update; nothing was taken back")
}

/// Every note in a window, oldest first. Voided ones are left out: they are kept
/// so that the correction is on the record, not so that the chart shows both.
pub fn select_sql(table: &str, from: Micros, to: Micros) -> String {
    format!(
        "SELECT cast(timestamp AS long) t, node, note FROM {table} \
         WHERE timestamp >= cast({from} AS timestamp) AND timestamp < cast({to} AS timestamp) \
         AND NOT voided \
         ORDER BY timestamp"
    )
}

pub async fn fetch(
    client: &Client,
    table: &str,
    from: Micros,
    to: Micros,
) -> Result<Vec<Annotation>> {
    let data = client.exec(&select_sql(table, from, to)).await?;
    let (t, n, note) = (
        data.require("t")?,
        data.require("node")?,
        data.require("note")?,
    );
    Ok(data
        .rows()
        .iter()
        .filter_map(|row| {
            Some(Annotation {
                at_ms: as_i64(row.get(t)?)? / 1_000,
                node: as_str(row.get(n)?).unwrap_or_default().to_string(),
                note: as_str(row.get(note)?)?.to_string(),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(text: &str) -> Annotation {
        Annotation {
            at_ms: 1_700_000_000_000,
            node: "terrasse".into(),
            note: text.into(),
        }
    }

    #[test]
    fn the_table_never_expires() {
        // An annotation outliving its data is useless; the other way round is
        // worse, so there is no TTL here at any setting.
        assert!(!create_table_ddl("annotations").contains("TTL"));
        assert!(create_table_ddl("annotations").contains("PARTITION BY YEAR"));
    }

    #[test]
    fn a_note_becomes_one_line_with_millisecond_time() {
        let line = ilp_line("annotations", &note("scale recalibrated"));
        assert_eq!(
            line,
            "annotations,node=terrasse note=\"scale recalibrated\",voided=false 1700000000000000000\n"
        );
    }

    #[test]
    fn a_note_without_a_node_is_filed_under_the_fleet() {
        let mut a = note("the archive starts here");
        a.node = String::new();
        assert!(ilp_line("annotations", &a).contains(",node=fleet "));
    }

    #[test]
    fn quotes_and_backslashes_survive_the_field() {
        assert_eq!(
            escape_note(r#"the "10 cm" jumper"#),
            r#"the \"10 cm\" jumper"#
        );
        assert_eq!(escape_note(r"a\b"), r"a\\b");
        // A newline would end the line and make the rest a second record.
        assert_eq!(escape_note("two\nlines"), "two lines");
        let line = ilp_line("annotations", &note("said \"no\"\nand left"));
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn an_empty_note_is_refused() {
        assert!(check(&note("")).is_err());
        assert!(check(&note("   ")).is_err());
    }

    #[test]
    fn a_note_that_is_really_a_log_is_refused() {
        assert!(check(&note(&"x".repeat(NOTE_MAX + 1))).is_err());
        assert!(check(&note(&"x".repeat(NOTE_MAX))).is_ok());
    }

    #[test]
    fn a_note_needs_a_time() {
        let mut a = note("something");
        a.at_ms = 0;
        assert!(check(&a).is_err());
    }

    #[test]
    fn a_note_is_written_standing() {
        // Not left to the column default: a row whose `voided` is whatever the
        // database felt like is a row nobody can reason about later.
        assert!(ilp_line("annotations", &note("x")).contains(",voided=false "));
    }

    #[test]
    fn the_column_can_be_added_to_a_table_that_predates_it() {
        // `CREATE TABLE IF NOT EXISTS` leaves an existing table exactly as it
        // was, so the migration cannot live in the CREATE.
        let ddl = add_voided_ddl("annotations");
        assert!(
            ddl.contains("ADD COLUMN IF NOT EXISTS voided BOOLEAN"),
            "{ddl}"
        );
        assert!(create_table_ddl("annotations").contains("voided BOOLEAN"));
    }

    #[test]
    fn a_voided_note_is_not_handed_out() {
        assert!(select_sql("annotations", 10, 20).contains("AND NOT voided"));
    }

    #[test]
    fn counting_tells_never_written_from_taken_back() {
        // The two differ by exactly the voided clause, and that difference is
        // what stops `void` from reporting success for a note that was never
        // there in the first place.
        let live = count_sql("annotations", 1_700_000_000_000, "terrasse", true).unwrap();
        let any = count_sql("annotations", 1_700_000_000_000, "terrasse", false).unwrap();
        assert!(live.contains("AND NOT voided"), "{live}");
        assert!(!any.contains("voided"), "{any}");
        assert!(
            live.contains("cast(1700000000000000 AS timestamp)"),
            "{live}"
        );
    }

    #[test]
    fn a_fleet_note_is_addressed_as_the_fleet() {
        // Empty means the fleet on the way in, so it has to mean the fleet on
        // the way back out too -- otherwise a fleet-wide note can be written
        // and never taken back.
        let sql = void_sql("annotations", 1_700_000_000_000, "").unwrap();
        assert!(sql.contains("node = 'fleet'"), "{sql}");
    }

    #[test]
    fn the_update_never_touches_the_timestamp() {
        // It could not anyway -- it is the designated column -- but a SET that
        // tried would fail at the database rather than here, and the reason
        // this function exists at all is that the timestamp cannot be corrected.
        let sql = void_sql("annotations", 1_700_000_000_000, "bad").unwrap();
        assert!(sql.contains("SET voided = true"), "{sql}");
        assert!(!sql.contains("SET timestamp"), "{sql}");
    }

    #[test]
    fn a_node_name_cannot_smuggle_sql() {
        assert!(void_sql("annotations", 1, "bad' OR '1'='1").is_err());
        assert!(count_sql("annotations", 1, "x; DROP TABLE readings--", true).is_err());
    }

    #[test]
    fn the_window_is_half_open_like_every_other_query() {
        let sql = select_sql("annotations", 10, 20);
        assert!(sql.contains("timestamp >= cast(10 AS timestamp)"), "{sql}");
        assert!(sql.contains("timestamp < cast(20 AS timestamp)"), "{sql}");
        assert!(sql.contains("ORDER BY timestamp"), "{sql}");
    }
}
