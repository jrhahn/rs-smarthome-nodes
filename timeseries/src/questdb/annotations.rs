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

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use super::client::{as_i64, as_str, Client};
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
         note VARCHAR\
         ) TIMESTAMP(timestamp) PARTITION BY YEAR"
    )
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

/// One note as a line of ILP.
pub fn ilp_line(table: &str, a: &Annotation) -> String {
    let node = if a.node.is_empty() { "fleet" } else { &a.node };
    format!(
        "{table},node={node} note=\"{note}\" {nanos}\n",
        table = super::writer::escape_tag(table),
        node = super::writer::escape_tag(node),
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

/// Write one note. Synchronous rather than batched with the readings: there are
/// a handful a year, and whoever wrote it wants to know it landed.
pub async fn insert(client: &Client, table: &str, a: &Annotation) -> Result<()> {
    check(a)?;
    client.write_ilp(&ilp_line(table, a)).await
}

/// Every note in a window, oldest first.
pub fn select_sql(table: &str, from: Micros, to: Micros) -> String {
    format!(
        "SELECT cast(timestamp AS long) t, node, note FROM {table} \
         WHERE timestamp >= cast({from} AS timestamp) AND timestamp < cast({to} AS timestamp) \
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
            "annotations,node=terrasse note=\"scale recalibrated\" 1700000000000000000\n"
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
    fn the_window_is_half_open_like_every_other_query() {
        let sql = select_sql("annotations", 10, 20);
        assert!(sql.contains("timestamp >= cast(10 AS timestamp)"), "{sql}");
        assert!(sql.contains("timestamp < cast(20 AS timestamp)"), "{sql}");
        assert!(sql.contains("ORDER BY timestamp"), "{sql}");
    }
}
