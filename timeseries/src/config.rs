//! Settings: one TOML file, with the two secrets also readable from a file of
//! their own.
//!
//! The file-per-secret detour exists because of where this runs. On the home
//! server the service is a NixOS unit, and anything written into a `.nix` file
//! lands in `/nix/store`, which is world-readable. So `password_file` points at
//! something systemd hands over instead (`LoadCredential=`, agenix, a plain
//! root-owned file), and the TOML keeps only the path.
//!
//! Everything has a default that works against a broker and a QuestDB on
//! localhost, so the smallest useful configuration file is an empty one.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    pub mqtt: Mqtt,
    pub questdb: QuestDb,
    pub web: Web,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Mqtt {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// Read the password from this file instead, trimmed of trailing newline.
    pub password_file: Option<PathBuf>,
    /// Must be unique on the broker: a second client announcing the same id
    /// kicks the first one off, and the two then take turns reconnecting.
    pub client_id: String,
    /// Topic root the fleet publishes under -- `node.namespace` in the
    /// firmware, `smarthome` for every node in `FLEET`.
    pub namespace: String,
    /// Where Home Assistant's retained discovery messages live. The bridge
    /// reads them for channel metadata; it never publishes there.
    pub discovery_prefix: String,
}

impl Default for Mqtt {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 1883,
            user: String::new(),
            password: String::new(),
            password_file: None,
            client_id: "smarthome-timeseries".into(),
            namespace: "smarthome".into(),
            discovery_prefix: "homeassistant".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct QuestDb {
    /// Base URL of QuestDB's HTTP endpoint -- ILP ingest (`/write`) and SQL
    /// (`/exec`) are both here.
    pub url: String,
    pub user: String,
    pub password: String,
    pub password_file: Option<PathBuf>,
    /// Base table. The rollup views are this plus `_1m` / `_1h` / `_1d`.
    pub table: String,
    /// Table for the nodes' online/offline transitions.
    pub status_table: String,
    /// Table for the notes that explain the readings. Never expired.
    pub annotations_table: String,
    /// How long to keep the raw readings, as a QuestDB TTL. Empty keeps them
    /// for ever.
    pub retention: String,
    /// How long to keep the coarse rollup views (`_1h`, `_1d`).
    ///
    /// Long rather than unlimited, and that is a QuestDB constraint rather than
    /// a preference: a materialized view's TTL can be *changed* with `ALTER
    /// MATERIALIZED VIEW ... SET TTL` but not cleared -- zero is rejected as
    /// "not an integer multiple of partition size" -- so a view created with a
    /// TTL carries one for ever. A number that can be altered later is worth
    /// more than an absence that cannot, and fifty years of `_1d` is about
    /// 566,000 rows.
    pub rollup_retention: String,
    /// Create and maintain the rollup materialized views.
    pub rollups: bool,
    /// How often the batch of buffered readings is flushed.
    ///
    /// This is the knob that sets steady-state database load: every commit
    /// fans out into a refresh of the `_1m` view and a WAL apply. The fleet
    /// publishes a reading a minute per channel at most, so there is nothing to
    /// gain from a tighter cadence than a few seconds.
    pub flush_interval_secs: u64,
    /// Flush early once this many readings are buffered, so a burst (the whole
    /// fleet reconnecting at once) does not sit in memory for a whole interval.
    pub batch_max: usize,
}

impl Default for QuestDb {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:9000".into(),
            user: String::new(),
            password: String::new(),
            password_file: None,
            table: "readings".into(),
            status_table: "node_status".into(),
            annotations_table: "annotations".into(),
            retention: "3y".into(),
            rollup_retention: "50y".into(),
            rollups: true,
            flush_interval_secs: 5,
            batch_max: 1000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Web {
    pub bind: SocketAddr,
}

impl Default for Web {
    fn default() -> Self {
        Self {
            // Loopback, not `0.0.0.0`: the dashboard has no authentication of
            // its own. Put it behind the home server's reverse proxy, the same
            // way Home Assistant is reached.
            bind: "127.0.0.1:8087".parse().expect("literal is a valid addr"),
        }
    }
}

impl Settings {
    /// Read a settings file, or take every default if the path is `None`.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut settings = match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .with_context(|| format!("reading settings from {}", p.display()))?;
                toml::from_str::<Settings>(&text)
                    .with_context(|| format!("parsing settings from {}", p.display()))?
            }
            None => Settings::default(),
        };
        settings.resolve_secrets()?;
        settings.validate()?;
        Ok(settings)
    }

    fn resolve_secrets(&mut self) -> Result<()> {
        if let Some(p) = self.mqtt.password_file.clone() {
            self.mqtt.password = read_secret(&p)?;
        }
        if let Some(p) = self.questdb.password_file.clone() {
            self.questdb.password = read_secret(&p)?;
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.mqtt.client_id.is_empty() {
            bail!("mqtt.client_id must not be empty");
        }
        if self.mqtt.namespace.is_empty() {
            bail!("mqtt.namespace must not be empty");
        }
        if !self.questdb.url.starts_with("http://") && !self.questdb.url.starts_with("https://") {
            bail!("questdb.url must start with http:// or https://");
        }
        if self.questdb.flush_interval_secs == 0 {
            bail!("questdb.flush_interval_secs must be at least 1");
        }
        if self.questdb.batch_max == 0 {
            bail!("questdb.batch_max must be at least 1");
        }
        // Parsed here so a typo fails at start-up with the list of units,
        // rather than at the first `ALTER TABLE` with whatever QuestDB makes
        // of it.
        Retention::parse(&self.questdb.retention)?;
        Retention::parse(&self.questdb.rollup_retention)?;
        Ok(())
    }

    /// The base URL without its trailing slash, so paths can be appended.
    pub fn questdb_base(&self) -> &str {
        self.questdb.url.trim_end_matches('/')
    }

    pub fn retention(&self) -> Retention {
        Retention::parse(&self.questdb.retention).expect("validated at load")
    }

    /// What the coarse rollup views are kept for.
    pub fn rollup_retention(&self) -> Retention {
        Retention::parse(&self.questdb.rollup_retention).expect("validated at load")
    }
}

fn read_secret(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading secret from {}", path.display()))?;
    Ok(raw.trim_end_matches(['\r', '\n']).to_string())
}

/// A QuestDB table TTL, normalised into the form its DDL wants.
///
/// QuestDB expires data a whole partition at a time, so a TTL finer than the
/// partition unit would simply never fire. Every table here is partitioned by
/// day or coarser, which is why minutes and seconds are not accepted at all --
/// `3m` would be read as three *months*, and a configuration that means
/// something different from what it says is worse than one that is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retention(Option<String>);

impl Retention {
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(Retention(None));
        }
        let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            bail!("retention {raw:?} must start with a number, e.g. 3y");
        }
        let count: u32 = digits.parse().context("retention count")?;
        if count == 0 {
            bail!("retention {raw:?} must be a positive number of periods");
        }
        let unit = raw[digits.len()..].trim().to_ascii_uppercase();
        let unit = match unit.as_str() {
            "H" | "HOUR" | "HOURS" => "HOURS",
            "D" | "DAY" | "DAYS" => "DAYS",
            "W" | "WEEK" | "WEEKS" => "WEEKS",
            "M" | "MONTH" | "MONTHS" => "MONTHS",
            "Y" | "YEAR" | "YEARS" => "YEARS",
            other => bail!(
                "retention unit {other:?} is not one of H(OURS), D(AYS), W(EEKS), \
                 M(ONTHS), Y(EARS) -- note that M is months, because a TTL shorter \
                 than a partition could never take effect"
            ),
        };
        Ok(Retention(Some(format!("{count} {unit}"))))
    }

    /// `" TTL 3 YEARS"`, or nothing at all when data is kept indefinitely.
    /// Written as a whole clause so the DDL builders can paste it in.
    pub fn clause(&self) -> String {
        match &self.0 {
            Some(ttl) => format!(" TTL {ttl}"),
            None => String::new(),
        }
    }

    pub fn is_set(&self) -> bool {
        self.0.is_some()
    }

    pub fn as_sql(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_a_working_localhost_setup() {
        let s = Settings::load(None).unwrap();
        assert_eq!(s.mqtt.port, 1883);
        assert_eq!(s.questdb.table, "readings");
        assert_eq!(s.retention().as_sql(), Some("3 YEARS"));
        assert_eq!(s.questdb_base(), "http://127.0.0.1:9000");
    }

    #[test]
    fn the_summaries_outlive_the_readings_by_default() {
        let s = Settings::load(None).unwrap();
        assert_eq!(s.retention().as_sql(), Some("3 YEARS"));
        assert_eq!(s.rollup_retention().as_sql(), Some("50 YEARS"));
    }

    #[test]
    fn trailing_slash_is_trimmed_from_the_questdb_url() {
        let mut s = Settings::default();
        s.questdb.url = "http://db:9000/".into();
        assert_eq!(s.questdb_base(), "http://db:9000");
    }

    #[test]
    fn retention_accepts_every_documented_spelling() {
        for (raw, want) in [
            ("3y", "3 YEARS"),
            ("3Y", "3 YEARS"),
            ("3 years", "3 YEARS"),
            ("48H", "48 HOURS"),
            ("30d", "30 DAYS"),
            ("4 weeks", "4 WEEKS"),
            ("6M", "6 MONTHS"),
        ] {
            assert_eq!(Retention::parse(raw).unwrap().as_sql(), Some(want), "{raw}");
        }
    }

    #[test]
    fn empty_retention_keeps_data_for_ever() {
        let r = Retention::parse("  ").unwrap();
        assert!(!r.is_set());
        assert_eq!(r.clause(), "");
    }

    #[test]
    fn retention_rejects_units_that_could_never_fire() {
        // Minutes and seconds are finer than any partition here, and `3m`
        // would silently mean months.
        assert!(Retention::parse("30s").is_err());
        assert!(Retention::parse("15min").is_err());
        assert!(Retention::parse("0d").is_err());
        assert!(Retention::parse("y").is_err());
    }

    #[test]
    fn the_ttl_clause_is_ready_to_paste_into_ddl() {
        assert_eq!(Retention::parse("3y").unwrap().clause(), " TTL 3 YEARS");
    }

    #[test]
    fn an_unknown_key_is_an_error_rather_than_a_silent_default() {
        // `deny_unknown_fields` everywhere: a misspelled setting that quietly
        // keeps the default is the kind of thing found months later, by which
        // time the data it would have kept is gone.
        let err = toml::from_str::<Settings>("[questdb]\nretenshun = \"3y\"\n").unwrap_err();
        assert!(err.to_string().contains("retenshun"), "{err}");
    }

    #[test]
    fn a_bad_retention_fails_at_load_not_at_first_alter() {
        let dir = std::env::temp_dir().join("smarthome-ts-cfg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.toml");
        std::fs::write(&path, "[questdb]\nretention = \"30s\"\n").unwrap();
        assert!(Settings::load(Some(&path)).is_err());
        std::fs::remove_file(&path).ok();
    }
}
