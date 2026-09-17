//! The handful of types the ingest side, the store and the web side all agree
//! on.
//!
//! Deliberately small: a reading is a node, a key, a number and a time, and
//! everything downstream of the MQTT parser works in those terms. The firmware
//! publishes exactly that and nothing more -- there is no schema to negotiate,
//! because the topic *is* the schema (`<namespace>/<node>/<key>`, carrying
//! `{"v":<number>,"t":<unix_ms>}`).

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Microseconds since the Unix epoch.
///
/// QuestDB's native timestamp resolution, and the unit every query in this
/// crate casts to before it leaves SQL -- parsing ISO-8601 back out of a JSON
/// response would be work done twice for a number the database already has.
pub type Micros = i64;

/// Now, in microseconds. Falls back to 0 before 1970, which cannot happen and
/// costs one `unwrap_or` to say so.
pub fn now_micros() -> Micros {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as Micros)
        .unwrap_or(0)
}

/// One measurement, on its way to the database.
///
/// `at` is when the sensor produced it where the node could say so, and when
/// the bridge saw the message otherwise. A node has no clock of its own -- the
/// ESP32-C3 has no RTC that survives losing power -- so it asks the home server
/// for the time once per publish round and walks each reading back by its own
/// age (`src/clock.rs` in the firmware). A round whose time sync failed, or a
/// node not yet reflashed, publishes without one and is dated on arrival, which
/// is what every reading used to be.
///
/// The difference matters in exactly one place, and it is the reason for the
/// whole arrangement: a reading that can date itself can be *replayed*. The
/// broker holds the last value of each topic retained, so a restart of this
/// service recovers the head of every series instead of discarding it as
/// undateable -- see `mqtt::handle` and `schema::alter_dedup_ddl`.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub node: String,
    pub sensor: String,
    pub value: f64,
    pub at: Micros,
}

/// A node's last will / birth message, which is a state and not a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Online,
    Offline,
}

impl Availability {
    pub fn as_str(self) -> &'static str {
        match self {
            Availability::Online => "online",
            Availability::Offline => "offline",
        }
    }
}

/// What Home Assistant's retained discovery message says about one channel.
///
/// The firmware already publishes this for Home Assistant's benefit
/// (`src/discovery.rs`), retained, one message per entity. Subscribing to it
/// gets the dashboard its axis labels and its human-readable names for free --
/// and keeps them in one place: add a sensor to the firmware and both consumers
/// learn about it without anything here being edited.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ChannelMeta {
    /// `"Luft Temperatur"`, from the discovery payload's `name`.
    pub name: String,
    /// `"°C"`, from `unit_of_meas`. Empty for a plain count.
    pub unit: String,
    /// `"temperature"`, from `dev_cla`. Empty where the firmware omits it.
    pub device_class: String,
    /// The node's display name, from the discovery payload's `dev.name`.
    pub node_name: String,
}

/// A channel as the API reports it: identity, metadata, and its last value.
#[derive(Debug, Clone, Serialize)]
pub struct Channel {
    pub node: String,
    pub sensor: String,
    #[serde(flatten)]
    pub meta: ChannelMeta,
    /// Last value seen on MQTT since this process started, if any. `None`
    /// before the node's first publish -- a battery node can be ten minutes
    /// away from saying anything.
    pub last_value: Option<f64>,
    /// When that value arrived, in milliseconds (what JavaScript's `Date`
    /// wants).
    pub last_at_ms: Option<i64>,
    /// `null` until the node's availability topic has been seen.
    pub online: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_after_2020() {
        // 2020-01-01, in microseconds. Guards against a unit slip (millis or
        // nanos here would be off by three orders of magnitude each way).
        assert!(now_micros() > 1_577_836_800_000_000);
        assert!(now_micros() < 32_503_680_000_000_000); // year 3000
    }
}
